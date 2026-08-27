use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use base64::prelude::*;
use bdk_core::spk_client::SyncRequest;
use bdk_esplora::{esplora_client, EsploraAsyncExt};
use bdk_sqlite::Store;
use bdk_wallet::bitcoin::bip32::{Fingerprint, Xpub};
use bdk_wallet::bitcoin::consensus::{deserialize, serialize};
use bdk_wallet::bitcoin::hashes::{sha256, Hash};
use bdk_wallet::bitcoin::hex::DisplayHex;
use bdk_wallet::bitcoin::sighash::SighashCache;
use bdk_wallet::bitcoin::{
    absolute::LockTime, Address, Amount, FeeRate, Network, OutPoint, Psbt, ScriptBuf, Transaction,
    TxOut, Txid,
};
use bdk_wallet::template::Bip84Public;
use bdk_wallet::{KeychainKind, LocalOutput, PersistedWallet, TxOrdering, Wallet};
use clap::{Args, Parser, Subcommand};
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey};
use futures::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

const ARTIFACT_VERSION: u32 = 1;
const MAX_STANDARD_WEIGHT_WU: u64 = 400_000;
const MAX_SIGNER_RESPONSE_BYTES: u64 = 1_048_576;
const MAX_POST_SYNC_TIP_LAG: u32 = 12;
const PREPARE_CONFIRMATION: &str = "fulfillment-stopped,prepared-inputs-excluded,inputs-compliant";
const BROADCAST_CONFIRMATION: &str = "exclusive-maintenance-window-active";

#[derive(Debug, Parser)]
#[command(
    name = "plank-provider-maintenance",
    about = "Prepare and broadcast bounded BDK provider maintenance transactions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect an offline provider-wallet snapshot without signing.
    Inspect(InspectArgs),
    /// Prepare and remotely sign one immutable maintenance transaction.
    Prepare(Box<PrepareArgs>),
    /// Revalidate and broadcast one prepared artifact.
    Broadcast(BroadcastArgs),
}

#[derive(Debug, Clone, Args)]
struct WalletArgs {
    /// Transactionally consistent offline SQLite snapshot. Its name must contain `.snapshot.`.
    #[arg(long)]
    wallet_db: PathBuf,
    /// Exact account xpub configured by fulfillment.
    #[arg(long)]
    xpub: Xpub,
    /// Exact master fingerprint configured by fulfillment; never inferred from the xpub.
    #[arg(long)]
    master_fingerprint: Fingerprint,
    /// Esplora base URL used for tip and outspend validation.
    #[arg(long, default_value = "https://mutinynet.com/api")]
    esplora_url: String,
}

#[derive(Debug, Args)]
struct InspectArgs {
    #[command(flatten)]
    wallet: WalletArgs,
    /// Minimum confirmations for an output to be eligible.
    #[arg(long, default_value_t = 6)]
    min_confirmations: u32,
    /// Number of largest confirmed outputs retained as a working reserve.
    #[arg(long, default_value_t = 2)]
    preserve_largest: usize,
    /// Planned maximum inputs per transaction.
    #[arg(long, default_value_t = 500)]
    max_inputs: usize,
}

#[derive(Debug, Args)]
struct PrepareArgs {
    #[command(flatten)]
    wallet: WalletArgs,
    /// Required frozen manifest of prepared-payment `txid:vout` entries, one per line.
    #[arg(long)]
    exclude_outpoints: PathBuf,
    /// Existing artifact directory. Inputs in prior artifacts are excluded automatically.
    #[arg(long)]
    artifact_dir: PathBuf,
    /// Reuse a snapshot that completed `inspect`; refresh only its chain tip and live outspends.
    #[arg(long)]
    reuse_synced_snapshot: bool,
    /// Destination address. It must be a revealed address owned by this wallet snapshot.
    #[arg(long)]
    destination: Address<NetworkUnchecked>,
    /// Repeat the exact destination address. The destination must also belong to this wallet snapshot.
    #[arg(long)]
    confirm_destination: Address<NetworkUnchecked>,
    /// Print and optionally save the exact unsigned plan without calling the signer.
    #[arg(long)]
    dry_run: bool,
    /// Create-only path for the unsigned plan. Valid only with `--dry-run`.
    #[arg(long)]
    plan_output: Option<PathBuf>,
    /// Repeat the unsigned transaction ID printed by `--dry-run` before signing.
    #[arg(long)]
    confirm_plan_txid: Option<Txid>,
    /// Immutable unsigned plan created by `--dry-run`. Required before signing.
    #[arg(long)]
    approved_plan: Option<PathBuf>,
    /// Full fulfillment signer endpoint, including `/v1/sign`. Required unless `--dry-run` is set.
    #[arg(long)]
    signer_url: Option<String>,
    /// Ed25519 PKCS#8 PEM used only to authenticate the signer request. Required unless `--dry-run` is set.
    #[arg(long)]
    signer_auth_key: Option<PathBuf>,
    /// Signer network selector. The fulfillment signer expects `mutinynet`.
    #[arg(long, default_value = "mutinynet")]
    signer_network: String,
    /// Minimum confirmations for selected inputs.
    #[arg(long, default_value_t = 6)]
    min_confirmations: u32,
    /// Retain this many of the largest confirmed wallet outputs.
    #[arg(long, default_value_t = 2)]
    preserve_largest: usize,
    /// Maximum inputs in this one transaction.
    #[arg(long, default_value_t = 500)]
    max_inputs: usize,
    /// Aim for outputs of this value; a final output drains the remainder.
    #[arg(long, default_value_t = 5_000_000)]
    target_output_sats: u64,
    /// Maximum outputs in the transaction.
    #[arg(long, default_value_t = 100)]
    max_outputs: usize,
    /// Fee rate in sat/vB.
    #[arg(long, default_value_t = 3)]
    fee_rate_sat_vb: u64,
    /// Hard cap on the final transaction fee.
    #[arg(long, default_value_t = 200_000)]
    max_fee_sats: u64,
    /// Hard cap below Bitcoin's 400,000-WU standardness limit.
    #[arg(long, default_value_t = 200_000)]
    max_weight_wu: u64,
    /// Must equal `fulfillment-stopped,prepared-inputs-excluded,inputs-compliant` before signing.
    #[arg(long)]
    confirm_maintenance: Option<String>,
}

#[derive(Debug, Args)]
struct BroadcastArgs {
    /// Immutable artifact produced by `prepare`.
    #[arg(long)]
    artifact: PathBuf,
    /// Esplora base URL used for outspend checks and publication.
    #[arg(long, default_value = "https://mutinynet.com/api")]
    esplora_url: String,
    /// Repeat the exact artifact txid.
    #[arg(long)]
    confirm_txid: Txid,
    /// Repeat the exact artifact fee in sats.
    #[arg(long)]
    confirm_fee_sats: u64,
    /// Must equal `exclusive-maintenance-window-active`.
    #[arg(long)]
    confirm_safe_to_broadcast: String,
}

// Clap needs the marker type in scope for a parsed-but-unchecked address.
use bdk_wallet::bitcoin::address::NetworkUnchecked;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InputRecord {
    outpoint: String,
    value_sats: u64,
    script_pubkey_hex: String,
    confirmation_height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OutputRecord {
    value_sats: u64,
    script_pubkey_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MaintenanceArtifact {
    version: u32,
    created_at_unix: u64,
    snapshot_tip_height: u32,
    snapshot_tip_hash: String,
    remote_tip_height: u32,
    script_sync_performed: bool,
    xpub: String,
    master_fingerprint: String,
    signer_network: String,
    destination: String,
    destination_script_hex: String,
    destination_keychain: String,
    destination_index: u32,
    exclusion_count: usize,
    exclusion_sha256: String,
    inputs: Vec<InputRecord>,
    outputs: Vec<OutputRecord>,
    psbt: String,
    signed_tx_hex: String,
    txid: String,
    fee_sats: u64,
    fee_rate_sat_vb: f64,
    weight_wu: u64,
    max_fee_sats: u64,
    max_weight_wu: u64,
}

#[derive(Debug, Serialize)]
struct WalletSummary {
    snapshot_tip_height: u32,
    snapshot_tip_hash: String,
    remote_tip_height: u32,
    tip_lag: u32,
    total_utxos: usize,
    total_sats: u64,
    eligible_confirmed_p2wpkh_utxos: usize,
    eligible_confirmed_p2wpkh_sats: u64,
    preserved_largest_utxos: usize,
    planned_input_utxos: usize,
    planned_batches: usize,
    suggested_consolidation_destination: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct UnsignedPlanReport {
    version: u32,
    unsigned_txid: String,
    snapshot_tip_height: u32,
    remote_tip_height: u32,
    script_sync_performed: bool,
    destination: String,
    destination_keychain: String,
    destination_index: u32,
    xpub: String,
    master_fingerprint: String,
    signer_network: String,
    exclusion_count: usize,
    exclusion_sha256: String,
    inputs: Vec<InputRecord>,
    outputs: Vec<OutputRecord>,
    psbt: String,
    fee_sats: u64,
    requested_fee_rate_sat_vb: u64,
    max_fee_sats: u64,
    max_weight_wu: u64,
}

type ProviderWallet = PersistedWallet<Store>;

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Inspect(args) => inspect(args).await,
        Command::Prepare(args) => prepare(*args).await,
        Command::Broadcast(args) => broadcast(args).await,
    }
}

async fn open_wallet(args: &WalletArgs) -> Result<(ProviderWallet, Store)> {
    validate_snapshot_path(&args.wallet_db)?;
    let db_path = args
        .wallet_db
        .to_str()
        .context("wallet snapshot path is not valid UTF-8")?;
    let mut store = Store::new(db_path)
        .await
        .with_context(|| format!("opening BDK snapshot {}", args.wallet_db.display()))?;
    let external = Bip84Public(args.xpub, args.master_fingerprint, KeychainKind::External);
    let internal = Bip84Public(args.xpub, args.master_fingerprint, KeychainKind::Internal);
    let wallet = Wallet::load()
        .descriptor(KeychainKind::External, Some(external))
        .descriptor(KeychainKind::Internal, Some(internal))
        .check_network(Network::Signet)
        .load_wallet_async(&mut store)
        .await?
        .context("snapshot does not contain the configured BDK wallet")?;
    Ok((wallet, store))
}

fn validate_snapshot_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading wallet snapshot metadata: {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "wallet snapshot must not be a symbolic link: {}",
        path.display()
    );
    ensure!(
        metadata.is_file(),
        "wallet snapshot does not exist: {}",
        path.display()
    );
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("wallet snapshot has no valid filename")?;
    ensure!(
        name.contains(".snapshot.") || name.ends_with(".snapshot"),
        "refusing a possible live wallet DB; snapshot filename must contain `.snapshot.`"
    );
    Ok(())
}

fn esplora_client(url: &str) -> Result<esplora_client::AsyncClient> {
    Ok(esplora_client::Builder::new(url.trim_end_matches('/'))
        .timeout(30)
        .build_async()?)
}

async fn sync_snapshot(
    wallet: &mut ProviderWallet,
    store: &mut Store,
    client: &esplora_client::AsyncClient,
) -> Result<u32> {
    let update = client
        .sync(wallet.start_sync_with_revealed_spks(), 5)
        .await
        .context("syncing revealed wallet scripts")?;
    wallet.apply_update(update)?;
    wallet.persist_async(store).await?;

    refresh_snapshot_tip(wallet, store, client).await
}

async fn refresh_snapshot_tip(
    wallet: &mut ProviderWallet,
    store: &mut Store,
    client: &esplora_client::AsyncClient,
) -> Result<u32> {
    // Esplora snapshots its chain tip before scanning scripts. A large wallet can take
    // many blocks to scan on Mutinynet, so refresh only the local chain after the
    // expensive transaction update. Selected outpoints still receive a separate live
    // outspend check immediately before signing.
    let chain_update = client
        .sync(
            SyncRequest::<()>::builder()
                .chain_tip(wallet.latest_checkpoint())
                .build(),
            1,
        )
        .await
        .context("refreshing wallet chain tip after script sync")?;
    wallet.apply_update(chain_update)?;
    wallet.persist_async(store).await?;
    Ok(wallet.latest_checkpoint().height())
}

fn eligible_outputs(wallet: &ProviderWallet, min_confirmations: u32) -> Result<Vec<LocalOutput>> {
    let tip = wallet.latest_checkpoint().height();
    let mut outputs = wallet
        .list_unspent()
        .filter(|output| {
            output
                .chain_position
                .confirmation_height_upper_bound()
                .is_some_and(|height| {
                    tip.saturating_sub(height).saturating_add(1) >= min_confirmations
                })
        })
        .collect::<Vec<_>>();
    ensure!(
        outputs
            .iter()
            .all(|output| output.txout.script_pubkey.is_p2wpkh()),
        "wallet contains an eligible non-P2WPKH output; refusing mixed-descriptor maintenance"
    );
    outputs.sort_by_key(|output| (output.txout.value.to_sat(), output.outpoint));
    Ok(outputs)
}

async fn inspect(args: InspectArgs) -> Result<()> {
    ensure!(args.max_inputs > 0, "max_inputs must be positive");
    let (mut wallet, mut store) = open_wallet(&args.wallet).await?;
    let client = esplora_client(&args.wallet.esplora_url)?;
    let local_tip = sync_snapshot(&mut wallet, &mut store, &client).await?;
    let remote_tip = client.get_height().await?;
    ensure!(
        remote_tip >= local_tip,
        "Esplora is behind the wallet snapshot"
    );
    let outputs = eligible_outputs(&wallet, args.min_confirmations)?;
    let selected_count = outputs.len().saturating_sub(args.preserve_largest);
    let suggested_keychain = [KeychainKind::Internal, KeychainKind::External]
        .into_iter()
        .find(|keychain| wallet.derivation_index(*keychain).is_some())
        .context("wallet has no revealed destination address")?;
    let summary = WalletSummary {
        snapshot_tip_height: local_tip,
        snapshot_tip_hash: wallet.latest_checkpoint().hash().to_string(),
        remote_tip_height: remote_tip,
        tip_lag: remote_tip - local_tip,
        total_utxos: wallet.list_unspent().count(),
        total_sats: wallet.balance().total().to_sat(),
        eligible_confirmed_p2wpkh_utxos: outputs.len(),
        eligible_confirmed_p2wpkh_sats: outputs
            .iter()
            .map(|output| output.txout.value.to_sat())
            .sum(),
        preserved_largest_utxos: args.preserve_largest.min(outputs.len()),
        planned_input_utxos: selected_count,
        planned_batches: selected_count.div_ceil(args.max_inputs),
        suggested_consolidation_destination: wallet
            .peek_address(suggested_keychain, 0)
            .address
            .to_string(),
    };
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn load_exclusions(path: &Path) -> Result<HashSet<OutPoint>> {
    let mut excluded = HashSet::new();
    for (line_number, raw) in fs::read_to_string(path)?.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let outpoint = OutPoint::from_str(line).with_context(|| {
            format!("invalid outpoint at {}:{}", path.display(), line_number + 1)
        })?;
        ensure!(
            excluded.insert(outpoint),
            "duplicate excluded outpoint: {outpoint}"
        );
    }
    Ok(excluded)
}

fn exclusion_digest(excluded: &HashSet<OutPoint>) -> String {
    let mut entries = excluded.iter().map(ToString::to_string).collect::<Vec<_>>();
    entries.sort_unstable();
    let canonical = if entries.is_empty() {
        String::new()
    } else {
        format!("{}\n", entries.join("\n"))
    };
    sha256::Hash::hash(canonical.as_bytes()).to_string()
}

fn read_artifacts(dir: &Path) -> Result<Vec<MaintenanceArtifact>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut artifacts = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("batch-")
            || path.extension().and_then(|ext| ext.to_str()) != Some("json")
        {
            continue;
        }
        let artifact: MaintenanceArtifact = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("reading prior artifact {}", path.display()))?;
        ensure!(
            artifact.version == ARTIFACT_VERSION,
            "unsupported prior artifact version"
        );
        artifacts.push(artifact);
    }
    artifacts.sort_by_key(|artifact| artifact.created_at_unix);
    Ok(artifacts)
}

async fn require_prior_artifacts_confirmed(
    client: &esplora_client::AsyncClient,
    artifacts: &[MaintenanceArtifact],
) -> Result<()> {
    for artifact in artifacts {
        let txid = Txid::from_str(&artifact.txid)?;
        let status = client
            .get_tx_status(&txid)
            .await
            .with_context(|| format!("checking prior transaction {txid}"))?;
        ensure!(
            status.confirmed,
            "prior maintenance transaction {txid} is not confirmed"
        );
    }
    Ok(())
}

async fn check_unspent(client: &esplora_client::AsyncClient, outpoints: &[OutPoint]) -> Result<()> {
    stream::iter(outpoints.iter().copied())
        .map(|outpoint| {
            let client = client.clone();
            async move {
                let status = client
                    .get_output_status(&outpoint.txid, u64::from(outpoint.vout))
                    .await
                    .with_context(|| format!("checking outspend for {outpoint}"))?
                    .with_context(|| format!("Esplora does not know outpoint {outpoint}"))?;
                ensure!(
                    !status.spent,
                    "selected outpoint is already spent: {outpoint}"
                );
                Ok::<_, anyhow::Error>(())
            }
        })
        .buffer_unordered(16)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(())
}

async fn prepare(args: PrepareArgs) -> Result<()> {
    ensure!(
        args.dry_run || args.plan_output.is_none(),
        "plan_output is valid only with --dry-run"
    );
    if args.dry_run {
        ensure!(
            args.plan_output.is_some(),
            "plan_output is required with --dry-run so the approved intent is durable"
        );
        ensure!(
            args.approved_plan.is_none(),
            "approved_plan is not valid with --dry-run"
        );
    } else {
        ensure!(
            args.confirm_maintenance.as_deref() == Some(PREPARE_CONFIRMATION),
            "maintenance acknowledgement must be `{PREPARE_CONFIRMATION}`"
        );
        ensure!(
            args.confirm_plan_txid.is_some(),
            "confirm_plan_txid is required before signing"
        );
        ensure!(
            args.approved_plan.is_some(),
            "approved_plan is required before signing"
        );
        ensure!(
            args.signer_url.is_some(),
            "signer_url is required before signing"
        );
        ensure!(
            args.signer_auth_key.is_some(),
            "signer_auth_key is required before signing"
        );
    }
    ensure!(args.fee_rate_sat_vb > 0, "fee rate must be positive");
    ensure!(
        (2..=1_000).contains(&args.max_inputs),
        "max_inputs must be between 2 and 1000"
    );
    ensure!(
        (1..=100).contains(&args.max_outputs),
        "max_outputs must be between 1 and 100"
    );
    ensure!(
        args.target_output_sats >= 1_000,
        "target output is unreasonably small"
    );
    ensure!(
        args.max_weight_wu <= MAX_STANDARD_WEIGHT_WU,
        "max_weight_wu exceeds the Bitcoin standardness limit"
    );
    ensure!(
        args.signer_network == "mutinynet",
        "only the Mutinynet signer is supported"
    );

    fs::create_dir_all(&args.artifact_dir)?;
    let mut permissions = fs::metadata(&args.artifact_dir)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&args.artifact_dir, permissions)?;

    let (mut wallet, mut store) = open_wallet(&args.wallet).await?;
    let client = esplora_client(&args.wallet.esplora_url)?;
    let snapshot_tip = if args.reuse_synced_snapshot {
        refresh_snapshot_tip(&mut wallet, &mut store, &client).await?
    } else {
        sync_snapshot(&mut wallet, &mut store, &client).await?
    };
    let remote_tip = client.get_height().await?;
    ensure!(
        remote_tip >= snapshot_tip,
        "Esplora is behind the wallet snapshot"
    );
    ensure!(
        remote_tip - snapshot_tip <= MAX_POST_SYNC_TIP_LAG,
        "wallet snapshot remains {} blocks behind Esplora after sync; maximum is {}",
        remote_tip - snapshot_tip,
        MAX_POST_SYNC_TIP_LAG
    );

    let prior_artifacts = read_artifacts(&args.artifact_dir)?;
    require_prior_artifacts_confirmed(&client, &prior_artifacts).await?;
    let mut excluded = load_exclusions(&args.exclude_outpoints)?;
    let exclusion_count = excluded.len();
    let exclusion_sha256 = exclusion_digest(&excluded);
    for artifact in &prior_artifacts {
        for input in &artifact.inputs {
            excluded.insert(OutPoint::from_str(&input.outpoint)?);
        }
        let txid = Txid::from_str(&artifact.txid)?;
        for vout in 0..artifact.outputs.len() {
            excluded.insert(OutPoint::new(txid, u32::try_from(vout)?));
        }
    }

    let mut eligible = eligible_outputs(&wallet, args.min_confirmations)?;
    ensure!(
        eligible.len() > args.preserve_largest,
        "no outputs remain after preserving the confirmed reserve"
    );
    let preserve_at = eligible.len() - args.preserve_largest;
    let preserved = eligible.split_off(preserve_at);
    let preserved_outpoints = preserved
        .iter()
        .map(|output| output.outpoint)
        .collect::<HashSet<_>>();
    eligible.retain(|output| {
        !excluded.contains(&output.outpoint) && !preserved_outpoints.contains(&output.outpoint)
    });
    let selected = eligible
        .into_iter()
        .take(args.max_inputs)
        .collect::<Vec<_>>();
    ensure!(
        selected.len() >= 2,
        "fewer than two eligible outputs remain; consolidation is complete"
    );

    let selected_outpoints = selected
        .iter()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    check_unspent(&client, &selected_outpoints).await?;

    let destination = args.destination.require_network(Network::Signet)?;
    let confirmed_destination = args.confirm_destination.require_network(Network::Signet)?;
    ensure!(
        destination == confirmed_destination,
        "confirmed destination does not match destination"
    );
    let destination_script = destination.script_pubkey();
    let (destination_keychain, destination_index) = wallet
        .spk_index()
        .index_of_spk(destination_script.clone())
        .copied()
        .context("destination does not belong to this wallet snapshot")?;
    let last_revealed = wallet
        .derivation_index(destination_keychain)
        .context("destination keychain has no revealed addresses")?;
    ensure!(
        destination_index <= last_revealed,
        "destination belongs to wallet lookahead but has not been revealed and persisted"
    );
    let selected_total = selected
        .iter()
        .try_fold(0_u64, |total, output| {
            total.checked_add(output.txout.value.to_sat())
        })
        .context("selected input value overflow")?;
    ensure!(
        selected_total > args.max_fee_sats.saturating_add(1_000),
        "selected inputs cannot cover the fee cap and a non-dust output"
    );
    let available_after_fee_cap = selected_total - args.max_fee_sats;
    let desired_outputs = usize::try_from(available_after_fee_cap / args.target_output_sats)
        .unwrap_or(usize::MAX)
        .clamp(1, args.max_outputs);

    let mut builder = wallet.build_tx();
    builder
        .ordering(TxOrdering::Untouched)
        .nlocktime(LockTime::ZERO)
        .add_utxos(&selected_outpoints)?
        .manually_selected_only()
        .drain_to(destination_script.clone())
        .fee_rate(FeeRate::from_sat_per_vb(args.fee_rate_sat_vb).context("invalid fee rate")?);
    for _ in 1..desired_outputs {
        builder.add_recipient(
            destination_script.clone(),
            Amount::from_sat(args.target_output_sats),
        );
    }
    let psbt = builder
        .finish()
        .context("building exact-input maintenance PSBT")?;
    ensure!(
        psbt.unsigned_tx.input.len() == selected.len(),
        "BDK added or removed an explicitly selected input"
    );
    ensure!(
        psbt.unsigned_tx.output.len() == desired_outputs,
        "unexpected maintenance output count"
    );
    ensure!(
        psbt.unsigned_tx
            .output
            .iter()
            .all(|output| output.script_pubkey == destination_script),
        "maintenance transaction contains an unexpected destination"
    );
    let psbt_fee = psbt.fee()?.to_sat();
    ensure!(
        psbt_fee <= args.max_fee_sats,
        "PSBT fee exceeds max_fee_sats"
    );

    let unsigned_txid = psbt.unsigned_tx.compute_txid();
    let plan = UnsignedPlanReport {
        version: ARTIFACT_VERSION,
        unsigned_txid: unsigned_txid.to_string(),
        snapshot_tip_height: snapshot_tip,
        remote_tip_height: remote_tip,
        script_sync_performed: !args.reuse_synced_snapshot,
        destination: destination.to_string(),
        destination_keychain: match destination_keychain {
            KeychainKind::External => "external".to_owned(),
            KeychainKind::Internal => "internal".to_owned(),
        },
        destination_index,
        xpub: args.wallet.xpub.to_string(),
        master_fingerprint: args.wallet.master_fingerprint.to_string(),
        signer_network: args.signer_network.clone(),
        exclusion_count,
        exclusion_sha256: exclusion_sha256.clone(),
        inputs: selected.iter().map(input_record).collect::<Result<_>>()?,
        outputs: psbt
            .unsigned_tx
            .output
            .iter()
            .map(|output| OutputRecord {
                value_sats: output.value.to_sat(),
                script_pubkey_hex: output.script_pubkey.as_bytes().to_lower_hex_string(),
            })
            .collect(),
        psbt: psbt.to_string(),
        fee_sats: psbt_fee,
        requested_fee_rate_sat_vb: args.fee_rate_sat_vb,
        max_fee_sats: args.max_fee_sats,
        max_weight_wu: args.max_weight_wu,
    };
    if args.dry_run {
        let bytes = serde_json::to_vec_pretty(&plan)?;
        if let Some(path) = &args.plan_output {
            write_create_only(path, &bytes)?;
            eprintln!("unsigned plan: {}", path.display());
        }
        println!("{}", String::from_utf8(bytes)?);
        return Ok(());
    }
    ensure!(
        args.confirm_plan_txid == Some(unsigned_txid),
        "confirmed plan txid does not match the rebuilt unsigned transaction"
    );
    let approved_plan_path = args
        .approved_plan
        .as_deref()
        .context("missing approved_plan")?;
    let approved_plan: UnsignedPlanReport = serde_json::from_slice(
        &fs::read(approved_plan_path)
            .with_context(|| format!("reading approved plan {}", approved_plan_path.display()))?,
    )?;
    ensure!(
        approved_plan.version == ARTIFACT_VERSION,
        "unsupported approved plan version"
    );
    ensure!(
        approved_plan.unsigned_txid == plan.unsigned_txid
            && approved_plan.psbt == plan.psbt
            && approved_plan.destination == plan.destination
            && approved_plan.destination_keychain == plan.destination_keychain
            && approved_plan.destination_index == plan.destination_index
            && approved_plan.xpub == plan.xpub
            && approved_plan.master_fingerprint == plan.master_fingerprint
            && approved_plan.signer_network == plan.signer_network
            && approved_plan.script_sync_performed == plan.script_sync_performed
            && approved_plan.exclusion_count == plan.exclusion_count
            && approved_plan.exclusion_sha256 == plan.exclusion_sha256
            && approved_plan.inputs == plan.inputs
            && approved_plan.outputs == plan.outputs
            && approved_plan.fee_sats == plan.fee_sats
            && approved_plan.requested_fee_rate_sat_vb == plan.requested_fee_rate_sat_vb
            && approved_plan.max_fee_sats == plan.max_fee_sats
            && approved_plan.max_weight_wu == plan.max_weight_wu,
        "approved plan does not match the exact transaction rebuilt for signing"
    );

    // Recheck immediately before the signer receives an irrevocably usable transaction.
    check_unspent(&client, &selected_outpoints).await?;
    let signed_tx = remote_sign(
        &psbt,
        &args.signer_network,
        args.signer_url.as_deref().context("missing signer_url")?,
        args.signer_auth_key
            .as_deref()
            .context("missing signer_auth_key")?,
    )
    .await?;
    verify_signed_transaction(&psbt, &signed_tx, &selected)?;

    let fee_sats = transaction_fee(&signed_tx, &selected)?;
    ensure!(
        fee_sats == psbt_fee,
        "signed transaction fee differs from PSBT fee"
    );
    ensure!(
        fee_sats <= args.max_fee_sats,
        "signed fee exceeds max_fee_sats"
    );
    let weight_wu = signed_tx.weight().to_wu();
    ensure!(
        weight_wu <= args.max_weight_wu,
        "signed transaction exceeds max_weight_wu"
    );
    ensure!(
        weight_wu <= MAX_STANDARD_WEIGHT_WU,
        "signed transaction is non-standard by weight"
    );

    let txid = signed_tx.compute_txid();
    let artifact = MaintenanceArtifact {
        version: ARTIFACT_VERSION,
        created_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        snapshot_tip_height: snapshot_tip,
        snapshot_tip_hash: wallet.latest_checkpoint().hash().to_string(),
        remote_tip_height: remote_tip,
        script_sync_performed: !args.reuse_synced_snapshot,
        xpub: args.wallet.xpub.to_string(),
        master_fingerprint: args.wallet.master_fingerprint.to_string(),
        signer_network: args.signer_network,
        destination: destination.to_string(),
        destination_script_hex: destination_script.as_bytes().to_lower_hex_string(),
        destination_keychain: match destination_keychain {
            KeychainKind::External => "external".to_owned(),
            KeychainKind::Internal => "internal".to_owned(),
        },
        destination_index,
        exclusion_count,
        exclusion_sha256,
        inputs: selected.iter().map(input_record).collect::<Result<_>>()?,
        outputs: signed_tx
            .output
            .iter()
            .map(|output| OutputRecord {
                value_sats: output.value.to_sat(),
                script_pubkey_hex: output.script_pubkey.as_bytes().to_lower_hex_string(),
            })
            .collect(),
        psbt: psbt.to_string(),
        signed_tx_hex: serialize(&signed_tx).to_lower_hex_string(),
        txid: txid.to_string(),
        fee_sats,
        fee_rate_sat_vb: fee_sats as f64 / signed_tx.vsize() as f64,
        weight_wu,
        max_fee_sats: args.max_fee_sats,
        max_weight_wu: args.max_weight_wu,
    };
    let artifact_path = args.artifact_dir.join(format!(
        "batch-{:03}-{txid}.json",
        prior_artifacts.len() + 1
    ));
    write_create_only(&artifact_path, &serde_json::to_vec_pretty(&artifact)?)?;
    println!("{}", serde_json::to_string_pretty(&artifact)?);
    eprintln!("artifact: {}", artifact_path.display());
    Ok(())
}

fn input_record(output: &LocalOutput) -> Result<InputRecord> {
    let confirmation_height = output
        .chain_position
        .confirmation_height_upper_bound()
        .context("selected output is unconfirmed")?;
    Ok(InputRecord {
        outpoint: output.outpoint.to_string(),
        value_sats: output.txout.value.to_sat(),
        script_pubkey_hex: output.txout.script_pubkey.as_bytes().to_lower_hex_string(),
        confirmation_height,
    })
}

#[derive(Serialize)]
struct SignRequest<'a> {
    psbt: String,
    network: &'a str,
}

#[derive(Deserialize)]
struct SignResponse {
    transaction: String,
}

async fn remote_sign(
    psbt: &Psbt,
    network: &str,
    signer_url: &str,
    auth_key_path: &Path,
) -> Result<Transaction> {
    let pem = fs::read_to_string(auth_key_path)
        .with_context(|| format!("reading signer auth key {}", auth_key_path.display()))?;
    let key = SigningKey::from_pkcs8_pem(&pem).context("parsing signer auth key")?;
    let request = SignRequest {
        psbt: psbt.to_string(),
        network,
    };
    let body = serde_json::to_vec(&request)?;
    let token = BASE64_STANDARD.encode(key.sign(&body).to_bytes());
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = http
        .post(signer_url)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .context("calling remote signer")?;
    let status = response.status();
    if let Some(length) = response.content_length() {
        ensure!(
            length <= MAX_SIGNER_RESPONSE_BYTES,
            "signer response is too large"
        );
    }
    let mut bytes = Vec::new();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.try_next().await? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_SIGNER_RESPONSE_BYTES as usize,
            "signer response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    ensure!(
        status.is_success(),
        "signer returned {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let response: SignResponse = serde_json::from_slice(&bytes)?;
    let tx_bytes =
        hex::decode(&response.transaction).context("signer returned non-hex transaction")?;
    deserialize(&tx_bytes).context("signer returned invalid transaction")
}

fn selected_prevouts(selected: &[LocalOutput]) -> Result<HashMap<OutPoint, TxOut>> {
    let mut prevouts = HashMap::new();
    for output in selected {
        ensure!(
            prevouts
                .insert(output.outpoint, output.txout.clone())
                .is_none(),
            "duplicate selected outpoint: {}",
            output.outpoint
        );
    }
    Ok(prevouts)
}

fn artifact_prevouts(artifact: &MaintenanceArtifact) -> Result<HashMap<OutPoint, TxOut>> {
    let mut prevouts = HashMap::new();
    for input in &artifact.inputs {
        let outpoint = OutPoint::from_str(&input.outpoint)?;
        let script_pubkey = ScriptBuf::from_hex(&input.script_pubkey_hex)?;
        ensure!(
            prevouts
                .insert(
                    outpoint,
                    TxOut {
                        value: Amount::from_sat(input.value_sats),
                        script_pubkey,
                    },
                )
                .is_none(),
            "artifact contains duplicate outpoint: {outpoint}"
        );
    }
    Ok(prevouts)
}

fn verify_signed_transaction(
    psbt: &Psbt,
    signed: &Transaction,
    selected: &[LocalOutput],
) -> Result<()> {
    verify_signed_transaction_with_prevouts(psbt, signed, selected_prevouts(selected)?)
}

fn verify_signed_transaction_with_prevouts(
    psbt: &Psbt,
    signed: &Transaction,
    prevouts: HashMap<OutPoint, TxOut>,
) -> Result<()> {
    ensure!(
        signed.input.len() == prevouts.len(),
        "signed input count changed"
    );
    let mut stripped = signed.clone();
    for input in &mut stripped.input {
        ensure!(
            input.script_sig.is_empty(),
            "signed transaction has a non-empty scriptSig"
        );
        input.witness.clear();
    }
    ensure!(
        stripped == psbt.unsigned_tx,
        "signer changed the unsigned transaction"
    );
    ensure!(
        signed.compute_txid() == psbt.unsigned_tx.compute_txid(),
        "signed transaction txid differs from the PSBT"
    );

    let mut consensus_prevouts = prevouts.clone();
    signed
        .verify(|outpoint| consensus_prevouts.remove(outpoint))
        .context("bitcoinconsensus rejected the signed transaction")?;
    ensure!(
        consensus_prevouts.is_empty(),
        "signed transaction did not spend every selected input"
    );

    let secp = bdk_wallet::bitcoin::secp256k1::Secp256k1::verification_only();
    let mut cache = SighashCache::new(signed);
    for (index, input) in signed.input.iter().enumerate() {
        let prevout = prevouts
            .get(&input.previous_output)
            .with_context(|| format!("missing prevout for input {index}"))?;
        ensure!(
            prevout.script_pubkey.is_p2wpkh(),
            "input {index} is not P2WPKH"
        );
        let witness = input.witness.iter().collect::<Vec<_>>();
        ensure!(
            witness.len() == 2,
            "input {index} has a non-canonical witness shape"
        );
        let signature = bdk_wallet::bitcoin::ecdsa::Signature::from_slice(witness[0])?;
        ensure!(
            signature.sighash_type == bdk_wallet::bitcoin::sighash::EcdsaSighashType::All,
            "input {index} does not use SIGHASH_ALL"
        );
        let public_key = bdk_wallet::bitcoin::PublicKey::from_slice(witness[1])?;
        let expected_script = ScriptBuf::new_p2wpkh(&public_key.wpubkey_hash()?);
        ensure!(
            expected_script == prevout.script_pubkey,
            "input {index} pubkey does not match prevout"
        );
        let sighash = cache.p2wpkh_signature_hash(
            index,
            &prevout.script_pubkey,
            prevout.value,
            signature.sighash_type,
        )?;
        let message = bdk_wallet::bitcoin::secp256k1::Message::from_digest(sighash.to_byte_array());
        secp.verify_ecdsa(&message, &signature.signature, &public_key.inner)
            .with_context(|| format!("invalid signature for input {index}"))?;
    }
    Ok(())
}

fn transaction_fee(signed: &Transaction, selected: &[LocalOutput]) -> Result<u64> {
    transaction_fee_with_prevouts(signed, &selected_prevouts(selected)?)
}

fn transaction_fee_with_prevouts(
    signed: &Transaction,
    prevouts: &HashMap<OutPoint, TxOut>,
) -> Result<u64> {
    let input_sats = signed.input.iter().try_fold(0_u64, |sum, input| {
        sum.checked_add(
            prevouts
                .get(&input.previous_output)
                .with_context(|| format!("missing prevout {}", input.previous_output))?
                .value
                .to_sat(),
        )
        .context("input value overflow")
    })?;
    let output_sats = signed.output.iter().try_fold(0_u64, |sum, output| {
        sum.checked_add(output.value.to_sat())
            .context("output value overflow")
    })?;
    input_sats
        .checked_sub(output_sats)
        .context("transaction outputs exceed inputs")
}

async fn broadcast(args: BroadcastArgs) -> Result<()> {
    ensure!(
        args.confirm_safe_to_broadcast == BROADCAST_CONFIRMATION,
        "broadcast acknowledgement must be `{BROADCAST_CONFIRMATION}`"
    );
    let artifact: MaintenanceArtifact = serde_json::from_slice(&fs::read(&args.artifact)?)?;
    ensure!(
        artifact.version == ARTIFACT_VERSION,
        "unsupported artifact version"
    );
    let artifact_txid = Txid::from_str(&artifact.txid)?;
    ensure!(
        args.confirm_txid == artifact_txid,
        "confirmed txid does not match artifact"
    );
    ensure!(
        args.confirm_fee_sats == artifact.fee_sats,
        "confirmed fee does not match artifact"
    );

    let psbt = Psbt::from_str(&artifact.psbt).context("artifact PSBT is invalid")?;
    let signed: Transaction = deserialize(&hex::decode(&artifact.signed_tx_hex)?)?;
    let prevouts = artifact_prevouts(&artifact)?;
    verify_signed_transaction_with_prevouts(&psbt, &signed, prevouts.clone())?;
    ensure!(
        signed.compute_txid() == artifact_txid,
        "artifact txid is inconsistent"
    );
    let fee = transaction_fee_with_prevouts(&signed, &prevouts)?;
    ensure!(fee == artifact.fee_sats, "artifact fee is inconsistent");
    ensure!(fee <= artifact.max_fee_sats, "artifact fee exceeds its cap");
    ensure!(
        signed.weight().to_wu() == artifact.weight_wu,
        "artifact weight is inconsistent"
    );
    ensure!(
        artifact.weight_wu <= artifact.max_weight_wu,
        "artifact exceeds its weight cap"
    );
    ensure!(
        artifact.weight_wu <= MAX_STANDARD_WEIGHT_WU,
        "artifact exceeds Bitcoin's standardness weight limit"
    );
    let destination_script = ScriptBuf::from_hex(&artifact.destination_script_hex)?;
    let destination = Address::<NetworkUnchecked>::from_str(&artifact.destination)?
        .require_network(Network::Signet)?;
    ensure!(
        destination.script_pubkey() == destination_script,
        "artifact destination address and script are inconsistent"
    );
    ensure!(
        signed
            .output
            .iter()
            .all(|output| output.script_pubkey == destination_script),
        "artifact contains an unexpected output destination"
    );
    ensure!(
        signed.output.len() == artifact.outputs.len(),
        "artifact output count is inconsistent"
    );
    for (index, (txout, record)) in signed.output.iter().zip(&artifact.outputs).enumerate() {
        ensure!(
            txout.value.to_sat() == record.value_sats
                && txout.script_pubkey.as_bytes().to_lower_hex_string() == record.script_pubkey_hex,
            "artifact output {index} is inconsistent"
        );
    }

    let client = esplora_client(&args.esplora_url)?;
    let statuses = stream::iter(prevouts.keys().copied())
        .map(|outpoint| {
            let client = client.clone();
            async move {
                let status = client
                    .get_output_status(&outpoint.txid, u64::from(outpoint.vout))
                    .await?
                    .with_context(|| format!("Esplora does not know outpoint {outpoint}"))?;
                Ok::<_, anyhow::Error>((outpoint, status))
            }
        })
        .buffer_unordered(16)
        .try_collect::<Vec<_>>()
        .await?;
    let mut same_spend = 0_usize;
    for (outpoint, status) in &statuses {
        if !status.spent {
            continue;
        }
        match status.txid {
            Some(spender) if spender == artifact_txid => same_spend += 1,
            Some(spender) => {
                bail!("outpoint {outpoint} was spent by competing transaction {spender}")
            }
            None => bail!("outpoint {outpoint} is spent without a reported spender"),
        }
    }
    if same_spend > 0 {
        ensure!(
            same_spend == statuses.len(),
            "Esplora reports a partial same-transaction spend; refusing ambiguous state"
        );
        let status = client.get_tx_status(&artifact_txid).await?;
        println!(
            "{}",
            serde_json::json!({
                "txid": artifact_txid,
                "publication": "already_known",
                "confirmed": status.confirmed,
                "block_height": status.block_height,
            })
        );
        return Ok(());
    }

    match client.broadcast(&signed).await {
        Ok(()) => {}
        Err(error) => match client.get_tx(&artifact_txid).await {
            Ok(Some(found)) if found == signed => {}
            _ => {
                return Err(error)
                    .context("broadcast failed and exact transaction was not observable")
            }
        },
    }
    println!(
        "{}",
        serde_json::json!({
            "txid": artifact_txid,
            "publication": "accepted",
            "fee_sats": fee,
        })
    );
    Ok(())
}

fn write_create_only(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("artifact path has no parent")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid artifact filename")?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp_path = parent.join(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::hard_link(&temp_path, path).with_context(|| {
            format!(
                "artifact already exists or cannot be linked: {}",
                path.display()
            )
        })?;
        OpenOptions::new().read(true).open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk_wallet::bitcoin::absolute::LockTime;
    use bdk_wallet::bitcoin::secp256k1::{Secp256k1, SecretKey};
    use bdk_wallet::bitcoin::transaction::Version;
    use bdk_wallet::bitcoin::{Sequence, TxIn, Witness};
    use tempfile::tempdir;

    #[test]
    fn snapshot_guard_rejects_a_possible_live_database() {
        let dir = tempdir().unwrap();
        let live = dir.path().join("provider.db");
        fs::write(&live, []).unwrap();
        assert!(validate_snapshot_path(&live).is_err());

        let snapshot = dir.path().join("provider.snapshot.db");
        fs::write(&snapshot, []).unwrap();
        validate_snapshot_path(&snapshot).unwrap();

        let disguised = dir.path().join("disguised.snapshot.db");
        std::os::unix::fs::symlink(&live, &disguised).unwrap();
        assert!(validate_snapshot_path(&disguised).is_err());
    }

    #[test]
    fn exclusions_accept_comments_and_reject_duplicates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("excluded.txt");
        let first = OutPoint::new(Txid::all_zeros(), 1);
        let second = OutPoint::new(Txid::from_byte_array([1; 32]), 2);
        fs::write(
            &path,
            format!("# durable inputs\n{first}\n{second} # owner\n"),
        )
        .unwrap();
        let parsed = load_exclusions(&path).unwrap();
        assert_eq!(parsed, HashSet::from([first, second]));
        assert_eq!(exclusion_digest(&parsed).len(), 64);

        fs::write(&path, format!("{first}\n{first}\n")).unwrap();
        assert!(load_exclusions(&path).is_err());
    }

    #[test]
    fn artifact_write_is_create_only_and_private() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("batch.json");
        write_create_only(&path, b"first").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_create_only(&path, b"second").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\n");
    }

    fn signed_p2wpkh_fixture() -> (Psbt, Transaction, HashMap<OutPoint, TxOut>) {
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[42; 32]).unwrap();
        let public_key = bdk_wallet::bitcoin::PublicKey::new(
            bdk_wallet::bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &secret_key),
        );
        let prevout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::new_p2wpkh(&public_key.wpubkey_hash().unwrap()),
        };
        let outpoint = OutPoint::new(Txid::from_byte_array([7; 32]), 0);
        let unsigned = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: outpoint,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(49_000),
                script_pubkey: prevout.script_pubkey.clone(),
            }],
        };
        let psbt = Psbt::from_unsigned_tx(unsigned.clone()).unwrap();
        let sighash_type = bdk_wallet::bitcoin::sighash::EcdsaSighashType::All;
        let sighash = SighashCache::new(&unsigned)
            .p2wpkh_signature_hash(0, &prevout.script_pubkey, prevout.value, sighash_type)
            .unwrap();
        let message = bdk_wallet::bitcoin::secp256k1::Message::from_digest(sighash.to_byte_array());
        let signature = bdk_wallet::bitcoin::ecdsa::Signature {
            signature: secp.sign_ecdsa(&message, &secret_key),
            sighash_type,
        };
        let mut signed = unsigned;
        signed.input[0].witness.push(signature.serialize());
        signed.input[0].witness.push(public_key.to_bytes());
        (psbt, signed, HashMap::from([(outpoint, prevout)]))
    }

    #[test]
    fn signed_transaction_requires_exact_psbt_and_valid_witness() {
        let (psbt, signed, prevouts) = signed_p2wpkh_fixture();
        verify_signed_transaction_with_prevouts(&psbt, &signed, prevouts.clone()).unwrap();

        let mut changed_output = signed.clone();
        changed_output.output[0].value = Amount::from_sat(48_999);
        assert!(
            verify_signed_transaction_with_prevouts(&psbt, &changed_output, prevouts.clone())
                .is_err()
        );

        let mut wrong_value = prevouts;
        wrong_value.values_mut().next().unwrap().value = Amount::from_sat(50_001);
        assert!(verify_signed_transaction_with_prevouts(&psbt, &signed, wrong_value).is_err());
    }
}
