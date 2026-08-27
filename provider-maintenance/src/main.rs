use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use base64::prelude::*;
use bdk_core::spk_client::SyncRequest;
use bdk_esplora::{esplora_client, EsploraAsyncExt};
use bdk_sqlite::Store;
use bdk_wallet::bitcoin::bip32::{ChildNumber, Fingerprint, Xpub};
use bdk_wallet::bitcoin::consensus::{deserialize, serialize};
use bdk_wallet::bitcoin::hashes::{sha256, Hash};
use bdk_wallet::bitcoin::hex::DisplayHex;
use bdk_wallet::bitcoin::sighash::SighashCache;
use bdk_wallet::bitcoin::{
    absolute::LockTime, Address, Amount, FeeRate, Network, OutPoint, Psbt, ScriptBuf, Transaction,
    TxOut, Txid, Witness,
};
use bdk_wallet::template::{Bip84Public, DescriptorTemplate};
use bdk_wallet::{KeychainKind, LocalOutput, PersistedWallet, TxOrdering, Wallet};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey};
use futures::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

const ARTIFACT_VERSION: u32 = 3;
const LEGACY_ARTIFACT_VERSION: u32 = 2;
const BATCH_SET_VERSION: u32 = 4;
const MAX_STANDARD_WEIGHT_WU: u64 = 400_000;
const DEFAULT_MAX_SIGNER_REQUEST_BYTES: usize = 900 * 1024;
const ABSOLUTE_MAX_SIGNER_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_SIGNER_RESPONSE_BYTES: u64 = 1_048_576;
const MAX_POST_SYNC_TIP_LAG: u32 = 12;
const UTXO_SYNC_PARALLEL_REQUESTS: usize = 16;
const PREPARE_CONFIRMATION: &str =
    "wallet-spenders-paused,prepared-inputs-excluded,inputs-compliant";
const BROADCAST_CONFIRMATION: &str = "exclusive-maintenance-window-active";
const FRESH_WALLET_CONFIRMATION: &str = "fresh-bip84-account-xpub-verified";
const BRIDGE_WALLET_CONFIRMATION: &str = "temporary-bridge-wallet-control-verified";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum MaintenanceMode {
    #[default]
    Consolidate,
    WalletReset,
    Bridge,
}

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
    /// Prepare and remotely sign an immutable maintenance plan.
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
    /// Frozen prepared-payment `txid:vout` manifest. Bridge mode requires zero entries.
    #[arg(long)]
    exclude_outpoints: PathBuf,
    /// Artifact directory. A new full-drain plan requires this directory to be empty.
    #[arg(long)]
    artifact_dir: PathBuf,
    /// Reuse a snapshot that completed `inspect`; refresh only its chain tip and live outspends.
    #[arg(long)]
    reuse_synced_snapshot: bool,
    /// Consolidate, reset to fresh BIP84 descriptors, or drain to a temporary bridge wallet.
    #[arg(long, value_enum, default_value_t)]
    mode: MaintenanceMode,
    /// Owned consolidation address or external Signet bridge address. Omit for wallet reset.
    #[arg(long)]
    destination: Option<Address<NetworkUnchecked>>,
    /// Repeat the exact consolidation, derived reset, or bridge destination address.
    #[arg(long)]
    confirm_destination: Option<Address<NetworkUnchecked>>,
    /// Fresh BIP84 account xpub. Required in wallet-reset mode.
    #[arg(long)]
    new_wallet_xpub: Option<Xpub>,
    /// Root fingerprint for the fresh BIP84 account. Required in wallet-reset mode.
    #[arg(long)]
    new_wallet_master_fingerprint: Option<Fingerprint>,
    /// Explicit Bitcoin network for the fresh account. Must be `signet` for Mutinynet.
    #[arg(long)]
    new_wallet_network: Option<Network>,
    /// Internal-chain derivation index in the fresh BIP84 account.
    #[arg(long)]
    new_wallet_internal_index: Option<u32>,
    /// Must equal `fresh-bip84-account-xpub-verified` in wallet-reset mode.
    #[arg(long)]
    confirm_fresh_wallet: Option<String>,
    /// Must equal `temporary-bridge-wallet-control-verified` in bridge mode.
    #[arg(long)]
    confirm_bridge_wallet: Option<String>,
    /// Require an exact eligible-source-UTXO drain. Bridge mode permits no exclusions.
    #[arg(long)]
    require_drain_all: bool,
    /// Exact output count across the complete wallet-reset batch set.
    #[arg(long)]
    reset_output_count: Option<usize>,
    /// Print and optionally save the exact unsigned plan without calling the signer.
    #[arg(long)]
    dry_run: bool,
    /// Create-only path for the unsigned plan. Valid only with `--dry-run`.
    #[arg(long)]
    plan_output: Option<PathBuf>,
    /// Repeat the unsigned transaction ID printed by `--dry-run` before signing.
    #[arg(long)]
    confirm_plan_txid: Option<Txid>,
    /// Repeat the version 4 batch-set digest before signing a wallet reset.
    #[arg(long)]
    confirm_batch_plan_digest: Option<String>,
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
    /// Maximum inputs in each transaction.
    #[arg(long, default_value_t = 500)]
    max_inputs: usize,
    /// Aim for outputs of this value; a final output drains the remainder.
    #[arg(long, default_value_t = 5_000_000)]
    target_output_sats: u64,
    /// Maximum outputs per transaction. Bridge mode requires an explicit value of 1.
    #[arg(long, default_value_t = 100)]
    max_outputs: usize,
    /// Fee rate in sat/vB.
    #[arg(long, default_value_t = 3)]
    fee_rate_sat_vb: u64,
    /// Hard cap on a single transaction, or the aggregate wallet-reset batch-set fee.
    #[arg(long, default_value_t = 200_000)]
    max_fee_sats: u64,
    /// Hard cap on each wallet-reset batch fee. The set is also capped by max_fee_sats.
    #[arg(long, default_value_t = 200_000)]
    max_fee_sats_per_batch: u64,
    /// Hard cap below Bitcoin's 400,000-WU standardness limit.
    #[arg(long, default_value_t = 200_000)]
    max_weight_wu: u64,
    /// Exact serialized signer JSON request cap per transaction.
    #[arg(long, default_value_t = DEFAULT_MAX_SIGNER_REQUEST_BYTES)]
    max_signer_request_bytes: usize,
    /// Must confirm that wallet spenders are paused and prepared inputs are excluded.
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
    /// Version 4 global batch-set manifest. Required for a batched wallet reset.
    #[arg(long)]
    batch_manifest: Option<PathBuf>,
    /// Repeat the exact version 4 batch-set plan digest.
    #[arg(long)]
    confirm_batch_plan_digest: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DescriptorIdentity {
    network: String,
    account_xpub: String,
    master_fingerprint: String,
    external_descriptor: String,
    internal_descriptor: String,
    descriptor_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct MaintenanceArtifact {
    version: u32,
    created_at_unix: u64,
    snapshot_tip_height: u32,
    snapshot_tip_hash: String,
    remote_tip_height: u32,
    known_utxo_sync_performed: bool,
    #[serde(default)]
    revealed_script_sync_performed: bool,
    #[serde(default)]
    bridge_control_verified: bool,
    #[serde(default)]
    maintenance_mode: MaintenanceMode,
    #[serde(default)]
    source_descriptor_identity: Option<DescriptorIdentity>,
    #[serde(default)]
    destination_descriptor_identity: Option<DescriptorIdentity>,
    #[serde(default)]
    require_drain_all: bool,
    #[serde(default)]
    eligible_input_count: usize,
    #[serde(default)]
    excluded_eligible_input_count: usize,
    #[serde(default)]
    preserved_input_count: usize,
    #[serde(default)]
    planned_output_count: usize,
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
    #[serde(default)]
    conservative_weight_wu: u64,
    max_fee_sats: u64,
    max_weight_wu: u64,
    #[serde(default)]
    batch_plan_digest: Option<String>,
    #[serde(default)]
    batch_index: Option<usize>,
    #[serde(default)]
    batch_count: Option<usize>,
    #[serde(default)]
    signer_request_bytes: Option<usize>,
    #[serde(default)]
    max_signer_request_bytes: Option<usize>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UnsignedPlanReport {
    version: u32,
    unsigned_txid: String,
    snapshot_tip_height: u32,
    remote_tip_height: u32,
    known_utxo_sync_performed: bool,
    revealed_script_sync_performed: bool,
    bridge_control_verified: bool,
    maintenance_mode: MaintenanceMode,
    source_descriptor_identity: DescriptorIdentity,
    destination_descriptor_identity: Option<DescriptorIdentity>,
    require_drain_all: bool,
    eligible_input_count: usize,
    excluded_eligible_input_count: usize,
    preserved_input_count: usize,
    planned_output_count: usize,
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
    conservative_weight_wu: u64,
    max_fee_sats: u64,
    max_weight_wu: u64,
}

impl UnsignedPlanReport {
    fn signing_commitment(&self) -> Self {
        let mut commitment = self.clone();
        commitment.snapshot_tip_height = 0;
        commitment.remote_tip_height = 0;
        commitment
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UnsignedBatchPlan {
    batch_index: usize,
    unsigned_txid: String,
    input_total_sats: u64,
    output_total_sats: u64,
    fee_sats: u64,
    planned_output_count: usize,
    signer_request_bytes: usize,
    conservative_weight_wu: u64,
    inputs: Vec<InputRecord>,
    outputs: Vec<OutputRecord>,
    psbt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BatchSetPlan {
    version: u32,
    plan_digest: String,
    snapshot_tip_height: u32,
    remote_tip_height: u32,
    known_utxo_sync_performed: bool,
    revealed_script_sync_performed: bool,
    maintenance_mode: MaintenanceMode,
    source_descriptor_identity: DescriptorIdentity,
    destination_descriptor_identity: DescriptorIdentity,
    require_drain_all: bool,
    eligible_input_count: usize,
    excluded_eligible_input_count: usize,
    preserved_input_count: usize,
    planned_output_count: usize,
    destination: String,
    destination_keychain: String,
    destination_index: u32,
    xpub: String,
    master_fingerprint: String,
    signer_network: String,
    exclusion_count: usize,
    exclusion_sha256: String,
    total_input_sats: u64,
    total_output_sats: u64,
    total_fee_sats: u64,
    requested_fee_rate_sat_vb: u64,
    max_inputs_per_batch: usize,
    max_outputs_per_batch: usize,
    max_total_fee_sats: u64,
    max_fee_sats_per_batch: u64,
    max_weight_wu_per_batch: u64,
    max_signer_request_bytes: usize,
    unsigned_txids: Vec<String>,
    batches: Vec<UnsignedBatchPlan>,
}

impl BatchSetPlan {
    fn signing_commitment(&self) -> Self {
        let mut commitment = self.clone();
        commitment.plan_digest.clear();
        commitment.snapshot_tip_height = 0;
        commitment.remote_tip_height = 0;
        commitment
    }

    fn computed_digest(&self) -> Result<String> {
        Ok(sha256::Hash::hash(&serde_json::to_vec(&self.signing_commitment())?).to_string())
    }

    fn seal(mut self) -> Result<Self> {
        self.plan_digest = self.computed_digest()?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BatchSetStatus {
    Signing,
    PartiallySigned,
    FullySigned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SignedBatchRecord {
    batch_index: usize,
    txid: String,
    artifact_file: String,
    artifact_sha256: String,
    materialized: bool,
    artifact: MaintenanceArtifact,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct BatchSetManifest {
    version: u32,
    plan_digest: String,
    status: BatchSetStatus,
    reserved_input_count: usize,
    reserved_inputs_sha256: String,
    plan: BatchSetPlan,
    signed_artifacts: Vec<SignedBatchRecord>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug)]
struct BuiltResetBatch {
    input_range: Range<usize>,
    psbt: Psbt,
    signer_request_bytes: usize,
    conservative_weight_wu: u64,
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
    let known_outpoints = wallet
        .list_unspent()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    ensure!(
        !known_outpoints.is_empty(),
        "wallet snapshot contains no known unspent outputs"
    );
    let update = client
        .sync(
            SyncRequest::<()>::builder()
                .chain_tip(wallet.latest_checkpoint())
                .outpoints(known_outpoints)
                .build(),
            UTXO_SYNC_PARALLEL_REQUESTS,
        )
        .await
        .context("syncing every known wallet UTXO")?;
    wallet.apply_update(update)?;
    wallet.persist_async(store).await?;

    refresh_snapshot_tip(wallet, store, client).await
}

async fn sync_all_revealed_scripts(
    wallet: &mut ProviderWallet,
    store: &mut Store,
    client: &esplora_client::AsyncClient,
) -> Result<u32> {
    let graph: &bdk_chain::TxGraph<bdk_core::ConfirmationBlockTime> = wallet.as_ref();
    let known_txids = graph
        .full_txs()
        .map(|transaction| transaction.txid)
        .collect::<Vec<_>>();
    let known_outpoints = wallet
        .list_unspent()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    ensure!(
        !known_txids.is_empty() && !known_outpoints.is_empty(),
        "source-wallet snapshot contains no persisted transaction inventory"
    );
    let request = wallet
        .start_sync_with_revealed_spks()
        .txids(known_txids)
        .outpoints(known_outpoints);
    let update = client
        .sync(request, UTXO_SYNC_PARALLEL_REQUESTS)
        .await
        .context("syncing every revealed source-wallet script and known transaction")?;
    wallet.apply_update(update)?;
    wallet.persist_async(store).await?;
    refresh_snapshot_tip(wallet, store, client).await
}

async fn refresh_snapshot_tip(
    wallet: &mut ProviderWallet,
    store: &mut Store,
    client: &esplora_client::AsyncClient,
) -> Result<u32> {
    // Esplora snapshots its chain tip before checking outpoints. Refresh only the local
    // chain after that bounded transaction update. Selected outpoints still receive a
    // separate live outspend check immediately before signing.
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

fn descriptor_identity_digest(
    network: &str,
    account_xpub: &str,
    master_fingerprint: &str,
    external_descriptor: &str,
    internal_descriptor: &str,
) -> String {
    let canonical = format!(
        "network={network}\naccount_xpub={account_xpub}\nmaster_fingerprint={master_fingerprint}\nexternal_descriptor={external_descriptor}\ninternal_descriptor={internal_descriptor}\n"
    );
    sha256::Hash::hash(canonical.as_bytes()).to_string()
}

fn bip84_descriptor_identity(
    account_xpub: Xpub,
    master_fingerprint: Fingerprint,
    network: Network,
    require_account_zero: bool,
) -> Result<DescriptorIdentity> {
    ensure!(
        account_xpub.network == network.into(),
        "BIP84 account xpub network does not match {network}"
    );
    if require_account_zero {
        ensure!(
            account_xpub.depth == 3
                && account_xpub.child_number == ChildNumber::Hardened { index: 0 },
            "fresh BIP84 xpub must be the account-zero key at depth 3"
        );
    }
    let external_descriptor = Bip84Public(account_xpub, master_fingerprint, KeychainKind::External)
        .build(network)?
        .0
        .to_string();
    let internal_descriptor = Bip84Public(account_xpub, master_fingerprint, KeychainKind::Internal)
        .build(network)?
        .0
        .to_string();
    let network = network.to_string();
    let account_xpub = account_xpub.to_string();
    let master_fingerprint = master_fingerprint.to_string();
    let descriptor_sha256 = descriptor_identity_digest(
        &network,
        &account_xpub,
        &master_fingerprint,
        &external_descriptor,
        &internal_descriptor,
    );
    Ok(DescriptorIdentity {
        network,
        account_xpub,
        master_fingerprint,
        external_descriptor,
        internal_descriptor,
        descriptor_sha256,
    })
}

fn validate_descriptor_identity(
    identity: &DescriptorIdentity,
    require_account_zero: bool,
) -> Result<(Xpub, Fingerprint, Network)> {
    let account_xpub = Xpub::from_str(&identity.account_xpub)
        .context("descriptor identity contains an invalid account xpub")?;
    let master_fingerprint = Fingerprint::from_str(&identity.master_fingerprint)
        .context("descriptor identity contains an invalid master fingerprint")?;
    let network = Network::from_str(&identity.network)
        .context("descriptor identity contains an invalid network")?;
    let rebuilt = bip84_descriptor_identity(
        account_xpub,
        master_fingerprint,
        network,
        require_account_zero,
    )?;
    ensure!(
        rebuilt == *identity,
        "descriptor identity does not match its BIP84 account parameters"
    );
    Ok((account_xpub, master_fingerprint, network))
}

fn derive_bip84_destination(
    identity: &DescriptorIdentity,
    keychain: KeychainKind,
    index: u32,
    require_account_zero: bool,
) -> Result<Address> {
    let (account_xpub, master_fingerprint, network) =
        validate_descriptor_identity(identity, require_account_zero)?;
    let descriptor = Bip84Public(account_xpub, master_fingerprint, keychain)
        .build(network)?
        .0;
    let script = descriptor
        .at_derivation_index(index)
        .context("deriving BIP84 destination index")?
        .script_pubkey();
    Address::from_script(&script, network).context("derived BIP84 script has no address")
}

fn verify_drain_all_outpoints(
    eligible: &[OutPoint],
    excluded: &HashSet<OutPoint>,
    selected: &[OutPoint],
) -> Result<usize> {
    let eligible_set = eligible.iter().copied().collect::<HashSet<_>>();
    ensure!(
        eligible_set.len() == eligible.len(),
        "eligible source-wallet set contains duplicate outpoints"
    );
    let selected_set = selected.iter().copied().collect::<HashSet<_>>();
    ensure!(
        selected_set.len() == selected.len(),
        "wallet-reset selection contains duplicate outpoints"
    );
    let expected = eligible_set
        .difference(excluded)
        .copied()
        .collect::<HashSet<_>>();
    let missing = expected.difference(&selected_set).count();
    let extra = selected_set.difference(&expected).count();
    ensure!(
        missing == 0 && extra == 0,
        "wallet-reset selection is not an exact drain: {missing} omitted, {extra} extra"
    );
    Ok(eligible_set.intersection(excluded).count())
}

struct ResolvedDestination {
    address: Address,
    keychain_label: String,
    index: u32,
    identity: Option<DescriptorIdentity>,
}

fn resolve_destination(
    args: &PrepareArgs,
    wallet: &ProviderWallet,
    source_identity: &DescriptorIdentity,
) -> Result<ResolvedDestination> {
    let confirmed = args
        .confirm_destination
        .clone()
        .context("confirm_destination is required")?
        .require_network(Network::Signet)?;
    match args.mode {
        MaintenanceMode::Consolidate => {
            ensure!(
                args.new_wallet_xpub.is_none()
                    && args.new_wallet_master_fingerprint.is_none()
                    && args.new_wallet_network.is_none()
                    && args.new_wallet_internal_index.is_none()
                    && args.confirm_fresh_wallet.is_none()
                    && args.confirm_bridge_wallet.is_none()
                    && args.reset_output_count.is_none()
                    && !args.require_drain_all,
                "wallet-reset options are not valid in consolidation mode"
            );
            let address = args
                .destination
                .clone()
                .context("destination is required in consolidation mode")?
                .require_network(Network::Signet)?;
            ensure!(
                address == confirmed,
                "confirmed destination does not match destination"
            );
            let script = address.script_pubkey();
            let (keychain, index) = wallet
                .spk_index()
                .index_of_spk(script)
                .copied()
                .context("destination does not belong to this wallet snapshot")?;
            let last_revealed = wallet
                .derivation_index(keychain)
                .context("destination keychain has no revealed addresses")?;
            ensure!(
                index <= last_revealed,
                "destination belongs to wallet lookahead but has not been revealed and persisted"
            );
            Ok(ResolvedDestination {
                address,
                keychain_label: match keychain {
                    KeychainKind::External => "external".to_owned(),
                    KeychainKind::Internal => "internal".to_owned(),
                },
                index,
                identity: Some(source_identity.clone()),
            })
        }
        MaintenanceMode::WalletReset => {
            ensure!(
                args.destination.is_none(),
                "wallet reset derives its destination; do not supply --destination"
            );
            ensure!(
                args.confirm_bridge_wallet.is_none(),
                "bridge acknowledgement is not valid in wallet-reset mode"
            );
            ensure!(
                args.confirm_fresh_wallet.as_deref() == Some(FRESH_WALLET_CONFIRMATION),
                "fresh-wallet acknowledgement must be `{FRESH_WALLET_CONFIRMATION}`"
            );
            let account_xpub = args
                .new_wallet_xpub
                .context("new_wallet_xpub is required in wallet-reset mode")?;
            let master_fingerprint = args
                .new_wallet_master_fingerprint
                .context("new_wallet_master_fingerprint is required in wallet-reset mode")?;
            let network = args
                .new_wallet_network
                .context("new_wallet_network is required in wallet-reset mode")?;
            ensure!(
                network == Network::Signet,
                "wallet reset only supports the Signet network used by Mutinynet"
            );
            ensure!(
                account_xpub.to_string() != source_identity.account_xpub,
                "fresh wallet xpub must differ from the source wallet xpub"
            );
            let index = args
                .new_wallet_internal_index
                .context("new_wallet_internal_index is required in wallet-reset mode")?;
            let identity =
                bip84_descriptor_identity(account_xpub, master_fingerprint, network, true)?;
            ensure!(
                identity.descriptor_sha256 != source_identity.descriptor_sha256,
                "fresh wallet descriptors must differ from the source descriptors"
            );
            let address = derive_bip84_destination(&identity, KeychainKind::Internal, index, true)?;
            ensure!(
                address == confirmed,
                "confirmed destination does not match the address derived from the fresh BIP84 account"
            );
            ensure!(
                wallet
                    .spk_index()
                    .index_of_spk(address.script_pubkey())
                    .is_none(),
                "derived fresh-wallet destination belongs to the source wallet"
            );
            Ok(ResolvedDestination {
                address,
                keychain_label: "internal".to_owned(),
                index,
                identity: Some(identity),
            })
        }
        MaintenanceMode::Bridge => {
            ensure!(
                args.new_wallet_xpub.is_none()
                    && args.new_wallet_master_fingerprint.is_none()
                    && args.new_wallet_network.is_none()
                    && args.new_wallet_internal_index.is_none()
                    && args.confirm_fresh_wallet.is_none()
                    && args.reset_output_count.is_none(),
                "fresh-wallet options are not valid in bridge mode"
            );
            ensure!(
                args.confirm_bridge_wallet.as_deref() == Some(BRIDGE_WALLET_CONFIRMATION),
                "bridge-wallet acknowledgement must be `{BRIDGE_WALLET_CONFIRMATION}`"
            );
            let address = args
                .destination
                .clone()
                .context("destination is required in bridge mode")?
                .require_network(Network::Signet)?;
            ensure!(
                address == confirmed,
                "confirmed bridge destination does not match destination"
            );
            ensure!(
                wallet
                    .spk_index()
                    .index_of_spk(address.script_pubkey())
                    .is_none(),
                "bridge destination unexpectedly belongs to the source wallet"
            );
            Ok(ResolvedDestination {
                address,
                keychain_label: "bridge".to_owned(),
                index: 0,
                identity: None,
            })
        }
    }
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
            (LEGACY_ARTIFACT_VERSION..=BATCH_SET_VERSION).contains(&artifact.version),
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
        if args.mode == MaintenanceMode::WalletReset {
            ensure!(
                args.confirm_batch_plan_digest.is_some(),
                "confirm_batch_plan_digest is required before signing a wallet-reset batch set"
            );
            ensure!(
                args.confirm_plan_txid.is_none(),
                "confirm_plan_txid is not valid for a wallet-reset batch set"
            );
        } else {
            ensure!(
                args.confirm_plan_txid.is_some(),
                "confirm_plan_txid is required before signing"
            );
            ensure!(
                args.confirm_batch_plan_digest.is_none(),
                "confirm_batch_plan_digest is valid only for a wallet-reset batch set"
            );
        }
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
        (1..=1_000).contains(&args.max_inputs),
        "max_inputs must be between 1 and 1000"
    );
    ensure!(
        (1..=100).contains(&args.max_outputs),
        "max_outputs must be between 1 and 100"
    );
    match args.mode {
        MaintenanceMode::Consolidate => ensure!(
            args.target_output_sats >= 1_000,
            "target output is unreasonably small"
        ),
        MaintenanceMode::WalletReset | MaintenanceMode::Bridge => {
            let mode = match args.mode {
                MaintenanceMode::WalletReset => "wallet-reset",
                MaintenanceMode::Bridge => "bridge",
                MaintenanceMode::Consolidate => unreachable!(),
            };
            ensure!(
                !args.reuse_synced_snapshot,
                "{mode} mode requires an exhaustive revealed-script sync; omit --reuse-synced-snapshot"
            );
            ensure!(
                args.require_drain_all,
                "{mode} mode requires --require-drain-all"
            );
            ensure!(
                args.preserve_largest == 0,
                "{mode} mode requires explicit --preserve-largest 0"
            );
            ensure!(
                args.min_confirmations == 1,
                "{mode} mode requires explicit --min-confirmations 1"
            );
            if args.mode == MaintenanceMode::WalletReset {
                let reset_output_count = args
                    .reset_output_count
                    .context("reset_output_count is required in wallet-reset mode")?;
                ensure!(
                    reset_output_count > 0,
                    "reset_output_count must be positive"
                );
                ensure!(
                    reset_output_count
                        <= args
                            .max_outputs
                            .checked_mul(args.max_inputs)
                            .context("reset output capacity overflow")?,
                    "reset_output_count cannot fit within the per-batch output cap"
                );
            } else {
                ensure!(
                    args.max_outputs == 1,
                    "bridge mode requires explicit --max-outputs 1"
                );
            }
        }
    }
    ensure!(
        args.max_weight_wu <= MAX_STANDARD_WEIGHT_WU,
        "max_weight_wu exceeds the Bitcoin standardness limit"
    );
    ensure!(
        (1..=ABSOLUTE_MAX_SIGNER_REQUEST_BYTES).contains(&args.max_signer_request_bytes),
        "max_signer_request_bytes must be between 1 and the original signer's 1 MiB request limit"
    );
    ensure!(
        args.signer_network == "mutinynet",
        "only the Mutinynet signer is supported"
    );

    fs::create_dir_all(&args.artifact_dir)?;
    let mut permissions = fs::metadata(&args.artifact_dir)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&args.artifact_dir, permissions)?;
    if args.mode == MaintenanceMode::Bridge
        || (args.mode == MaintenanceMode::WalletReset && args.dry_run)
    {
        ensure!(
            fs::read_dir(&args.artifact_dir)?.next().is_none(),
            "a new full-drain plan requires a completely empty artifact directory"
        );
    }

    let (mut wallet, mut store) = open_wallet(&args.wallet).await?;
    let source_identity = bip84_descriptor_identity(
        args.wallet.xpub,
        args.wallet.master_fingerprint,
        Network::Signet,
        false,
    )?;
    ensure!(
        wallet.public_descriptor(KeychainKind::External).to_string()
            == source_identity.external_descriptor
            && wallet.public_descriptor(KeychainKind::Internal).to_string()
                == source_identity.internal_descriptor,
        "source descriptor identity does not match the loaded wallet snapshot"
    );
    let client = esplora_client(&args.wallet.esplora_url)?;
    let snapshot_tip = match args.mode {
        MaintenanceMode::WalletReset | MaintenanceMode::Bridge => {
            sync_all_revealed_scripts(&mut wallet, &mut store, &client).await?
        }
        MaintenanceMode::Consolidate if args.reuse_synced_snapshot => {
            refresh_snapshot_tip(&mut wallet, &mut store, &client).await?
        }
        MaintenanceMode::Consolidate => sync_snapshot(&mut wallet, &mut store, &client).await?,
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
    if args.mode == MaintenanceMode::Bridge
        || (args.mode == MaintenanceMode::WalletReset && args.dry_run)
    {
        ensure!(
            prior_artifacts.is_empty(),
            "a new full-drain plan requires an empty artifact directory"
        );
    }
    if args.mode != MaintenanceMode::WalletReset {
        require_prior_artifacts_confirmed(&client, &prior_artifacts).await?;
    }
    let mandatory_excluded = load_exclusions(&args.exclude_outpoints)?;
    if args.mode == MaintenanceMode::Bridge {
        ensure!(
            mandatory_excluded.is_empty(),
            "bridge mode requires an explicitly supplied empty exclusion manifest"
        );
    }
    let exclusion_count = mandatory_excluded.len();
    let exclusion_sha256 = exclusion_digest(&mandatory_excluded);
    let mut excluded = mandatory_excluded.clone();
    if args.mode == MaintenanceMode::Consolidate {
        for artifact in &prior_artifacts {
            for input in &artifact.inputs {
                excluded.insert(OutPoint::from_str(&input.outpoint)?);
            }
            let txid = Txid::from_str(&artifact.txid)?;
            for vout in 0..artifact.outputs.len() {
                excluded.insert(OutPoint::new(txid, u32::try_from(vout)?));
            }
        }
    }

    let mut eligible = eligible_outputs(&wallet, args.min_confirmations)?;
    let eligible_input_count = eligible.len();
    if args.mode == MaintenanceMode::Bridge {
        let total_unspent = wallet.list_unspent().count();
        ensure!(
            eligible_input_count == total_unspent,
            "bridge mode requires every source-wallet UTXO to be confirmed: {eligible_input_count} confirmed of {total_unspent} total"
        );
    }
    let (selected, excluded_eligible_input_count, preserved_input_count) = match args.mode {
        MaintenanceMode::Consolidate => {
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
            let excluded_eligible_input_count = eligible
                .iter()
                .filter(|output| excluded.contains(&output.outpoint))
                .count();
            eligible.retain(|output| {
                !excluded.contains(&output.outpoint)
                    && !preserved_outpoints.contains(&output.outpoint)
            });
            let selected = eligible
                .into_iter()
                .take(args.max_inputs)
                .collect::<Vec<_>>();
            ensure!(
                selected.len() >= 2,
                "fewer than two eligible outputs remain; consolidation is complete"
            );
            (selected, excluded_eligible_input_count, preserved.len())
        }
        MaintenanceMode::WalletReset | MaintenanceMode::Bridge => {
            let eligible_outpoints = eligible
                .iter()
                .map(|output| output.outpoint)
                .collect::<Vec<_>>();
            let selected = eligible
                .into_iter()
                .filter(|output| !mandatory_excluded.contains(&output.outpoint))
                .collect::<Vec<_>>();
            ensure!(
                !selected.is_empty(),
                "full-drain mode has no confirmed source outputs"
            );
            if args.mode == MaintenanceMode::Bridge {
                ensure!(
                    selected.len() <= args.max_inputs,
                    "bridge mode needs {} inputs, which exceeds max_inputs {}; refuse a partial drain",
                    selected.len(),
                    args.max_inputs
                );
            }
            let selected_outpoints = selected
                .iter()
                .map(|output| output.outpoint)
                .collect::<Vec<_>>();
            let excluded_eligible_input_count = verify_drain_all_outpoints(
                &eligible_outpoints,
                &mandatory_excluded,
                &selected_outpoints,
            )?;
            (selected, excluded_eligible_input_count, 0)
        }
    };

    let selected_outpoints = selected
        .iter()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    check_unspent(&client, &selected_outpoints).await?;

    let resolved_destination = resolve_destination(&args, &wallet, &source_identity)?;
    let destination = resolved_destination.address;
    let destination_script = destination.script_pubkey();
    let destination_keychain = resolved_destination.keychain_label;
    let destination_index = resolved_destination.index;
    let destination_identity = resolved_destination.identity;
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
    if args.mode == MaintenanceMode::WalletReset {
        return prepare_wallet_reset_batch_set(
            &args,
            &mut wallet,
            &client,
            snapshot_tip,
            remote_tip,
            source_identity,
            destination_identity.context("wallet reset has no destination identity")?,
            destination,
            destination_keychain,
            destination_index,
            destination_script,
            eligible_input_count,
            excluded_eligible_input_count,
            preserved_input_count,
            exclusion_count,
            exclusion_sha256,
            selected,
        )
        .await;
    }
    let available_after_fee_cap = selected_total - args.max_fee_sats;
    let (desired_outputs, fixed_output_sats) = match args.mode {
        MaintenanceMode::Consolidate => (
            usize::try_from(available_after_fee_cap / args.target_output_sats)
                .unwrap_or(usize::MAX)
                .clamp(1, args.max_outputs),
            args.target_output_sats,
        ),
        MaintenanceMode::WalletReset => unreachable!("wallet reset uses a batch-set plan"),
        MaintenanceMode::Bridge => (1, available_after_fee_cap),
    };

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
            Amount::from_sat(fixed_output_sats),
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
    let conservative_weight_wu = conservative_signed_p2wpkh_weight(&psbt.unsigned_tx);
    ensure!(
        conservative_weight_wu <= args.max_weight_wu,
        "conservative signed P2WPKH weight estimate exceeds max_weight_wu"
    );
    ensure!(
        conservative_weight_wu <= MAX_STANDARD_WEIGHT_WU,
        "conservative signed P2WPKH weight estimate exceeds the Bitcoin standardness limit"
    );

    let unsigned_txid = psbt.unsigned_tx.compute_txid();
    let plan = UnsignedPlanReport {
        version: ARTIFACT_VERSION,
        unsigned_txid: unsigned_txid.to_string(),
        snapshot_tip_height: snapshot_tip,
        remote_tip_height: remote_tip,
        known_utxo_sync_performed: !args.reuse_synced_snapshot,
        revealed_script_sync_performed: matches!(
            args.mode,
            MaintenanceMode::WalletReset | MaintenanceMode::Bridge
        ),
        bridge_control_verified: args.mode == MaintenanceMode::Bridge,
        maintenance_mode: args.mode,
        source_descriptor_identity: source_identity.clone(),
        destination_descriptor_identity: destination_identity.clone(),
        require_drain_all: args.require_drain_all,
        eligible_input_count,
        excluded_eligible_input_count,
        preserved_input_count,
        planned_output_count: desired_outputs,
        destination: destination.to_string(),
        destination_keychain: destination_keychain.clone(),
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
        conservative_weight_wu,
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
        approved_plan.signing_commitment() == plan.signing_commitment(),
        "approved plan does not match the exact transaction and safety parameters rebuilt for signing"
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
        args.max_signer_request_bytes,
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
    ensure!(
        weight_wu <= conservative_weight_wu,
        "signed transaction exceeds the conservative P2WPKH weight estimate"
    );

    let txid = signed_tx.compute_txid();
    let artifact = MaintenanceArtifact {
        version: ARTIFACT_VERSION,
        created_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        snapshot_tip_height: snapshot_tip,
        snapshot_tip_hash: wallet.latest_checkpoint().hash().to_string(),
        remote_tip_height: remote_tip,
        known_utxo_sync_performed: !args.reuse_synced_snapshot,
        revealed_script_sync_performed: matches!(
            args.mode,
            MaintenanceMode::WalletReset | MaintenanceMode::Bridge
        ),
        bridge_control_verified: args.mode == MaintenanceMode::Bridge,
        maintenance_mode: args.mode,
        source_descriptor_identity: Some(source_identity),
        destination_descriptor_identity: destination_identity,
        require_drain_all: args.require_drain_all,
        eligible_input_count,
        excluded_eligible_input_count,
        preserved_input_count,
        planned_output_count: desired_outputs,
        xpub: args.wallet.xpub.to_string(),
        master_fingerprint: args.wallet.master_fingerprint.to_string(),
        signer_network: args.signer_network,
        destination: destination.to_string(),
        destination_script_hex: destination_script.as_bytes().to_lower_hex_string(),
        destination_keychain,
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
        conservative_weight_wu,
        max_fee_sats: args.max_fee_sats,
        max_weight_wu: args.max_weight_wu,
        batch_plan_digest: None,
        batch_index: None,
        batch_count: None,
        signer_request_bytes: None,
        max_signer_request_bytes: None,
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

fn build_reset_psbt_with_fixed_outputs(
    wallet: &mut ProviderWallet,
    selected: &[LocalOutput],
    destination_script: &ScriptBuf,
    output_count: usize,
    fixed_output_sats: u64,
    fee_rate_sat_vb: u64,
) -> Result<Psbt> {
    ensure!(!selected.is_empty(), "wallet-reset batch has no inputs");
    ensure!(output_count > 0, "wallet-reset batch has no outputs");
    let selected_outpoints = selected
        .iter()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    let mut builder = wallet.build_tx();
    builder
        .ordering(TxOrdering::Untouched)
        .nlocktime(LockTime::ZERO)
        .add_utxos(&selected_outpoints)?
        .manually_selected_only()
        .drain_to(destination_script.clone())
        .fee_rate(FeeRate::from_sat_per_vb(fee_rate_sat_vb).context("invalid fee rate")?);
    for _ in 1..output_count {
        builder.add_recipient(
            destination_script.clone(),
            Amount::from_sat(fixed_output_sats),
        );
    }
    let psbt = builder
        .finish()
        .context("building wallet-reset batch PSBT")?;
    ensure!(
        psbt.unsigned_tx.input.len() == selected.len(),
        "BDK changed the exact wallet-reset batch input set"
    );
    ensure!(
        psbt.unsigned_tx.output.len() == output_count,
        "BDK changed the exact wallet-reset batch output count"
    );
    ensure!(
        psbt.unsigned_tx
            .output
            .iter()
            .all(|output| output.script_pubkey == *destination_script),
        "wallet-reset batch contains an unexpected destination"
    );
    Ok(psbt)
}

fn build_reset_psbt(
    wallet: &mut ProviderWallet,
    selected: &[LocalOutput],
    destination_script: &ScriptBuf,
    output_count: usize,
    fee_rate_sat_vb: u64,
) -> Result<Psbt> {
    let selected_total = selected.iter().try_fold(0_u64, |sum, output| {
        sum.checked_add(output.txout.value.to_sat())
            .context("wallet-reset batch input value overflow")
    })?;
    let provisional = build_reset_psbt_with_fixed_outputs(
        wallet,
        selected,
        destination_script,
        output_count,
        1_000,
        fee_rate_sat_vb,
    )?;
    let fee_sats = provisional.fee()?.to_sat();
    let distributable = selected_total
        .checked_sub(fee_sats)
        .context("wallet-reset batch cannot pay its fee")?;
    let fixed_output_sats = distributable
        .checked_div(u64::try_from(output_count).context("invalid output count")?)
        .context("wallet-reset batch has zero outputs")?;
    ensure!(
        fixed_output_sats >= 1_000,
        "wallet-reset batch cannot fund {output_count} outputs of at least 1000 sats"
    );
    let psbt = build_reset_psbt_with_fixed_outputs(
        wallet,
        selected,
        destination_script,
        output_count,
        fixed_output_sats,
        fee_rate_sat_vb,
    )?;
    ensure!(
        psbt.fee()?.to_sat() == fee_sats,
        "wallet-reset batch fee changed while distributing outputs"
    );
    Ok(psbt)
}

fn batch_fits_caps(
    psbt: &Psbt,
    signer_network: &str,
    max_fee_sats: u64,
    max_weight_wu: u64,
    max_signer_request_bytes: usize,
) -> Result<Option<(usize, u64)>> {
    let signer_request_bytes = signer_request_body(psbt, signer_network)?.len();
    let conservative_weight_wu = conservative_signed_p2wpkh_weight(&psbt.unsigned_tx);
    let fee_sats = psbt.fee()?.to_sat();
    if signer_request_bytes > max_signer_request_bytes
        || conservative_weight_wu > max_weight_wu
        || conservative_weight_wu > MAX_STANDARD_WEIGHT_WU
        || fee_sats > max_fee_sats
    {
        return Ok(None);
    }
    Ok(Some((signer_request_bytes, conservative_weight_wu)))
}

fn distribute_reset_outputs(
    total: usize,
    batches: usize,
    per_batch_max: usize,
) -> Result<Vec<usize>> {
    ensure!(batches > 0, "wallet-reset batch count must be positive");
    ensure!(
        total >= batches,
        "wallet reset needs at least one destination output per batch"
    );
    let base = total / batches;
    let remainder = total % batches;
    let counts = (0..batches)
        .map(|index| base + usize::from(index < remainder))
        .collect::<Vec<_>>();
    ensure!(
        counts
            .iter()
            .all(|count| (1..=per_batch_max).contains(count)),
        "wallet-reset output count does not fit the per-batch output cap"
    );
    ensure!(
        counts.iter().sum::<usize>() == total,
        "wallet-reset output distribution changed the exact total"
    );
    Ok(counts)
}

fn distribute_reset_outputs_by_value(
    total: usize,
    batch_values: &[u64],
    per_batch_max: usize,
) -> Result<Vec<usize>> {
    ensure!(!batch_values.is_empty(), "wallet reset has no batches");
    ensure!(
        total >= batch_values.len(),
        "wallet reset needs at least one destination output per batch"
    );
    ensure!(
        total
            <= batch_values
                .len()
                .checked_mul(per_batch_max)
                .context("output capacity overflow")?,
        "wallet reset exceeds aggregate per-batch output capacity"
    );
    let total_value = batch_values.iter().try_fold(0_u128, |sum, value| {
        sum.checked_add(u128::from(*value))
            .context("batch value overflow")
    })?;
    ensure!(total_value > 0, "wallet reset has no input value");
    let mut counts = vec![1_usize; batch_values.len()];
    let distributable = total - counts.len();
    let mut assigned = 0_usize;
    let mut remainders = Vec::with_capacity(batch_values.len());
    for (index, value) in batch_values.iter().enumerate() {
        let numerator = u128::try_from(distributable)? * u128::from(*value);
        let quota = usize::try_from(numerator / total_value)?;
        let allocation = quota.min(per_batch_max - 1);
        counts[index] += allocation;
        assigned += allocation;
        remainders.push((numerator % total_value, *value, index));
    }
    let mut remaining = distributable - assigned;
    remainders.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    while remaining > 0 {
        let mut progressed = false;
        for (_, _, index) in &remainders {
            if counts[*index] == per_batch_max {
                continue;
            }
            counts[*index] += 1;
            remaining -= 1;
            progressed = true;
            if remaining == 0 {
                break;
            }
        }
        ensure!(progressed, "wallet-reset output capacity was exhausted");
    }
    ensure!(
        counts.iter().sum::<usize>() == total
            && counts
                .iter()
                .all(|count| (1..=per_batch_max).contains(count)),
        "value-proportional output distribution changed the exact total"
    );
    Ok(counts)
}

fn build_reset_batches(
    wallet: &mut ProviderWallet,
    selected: &mut Vec<LocalOutput>,
    destination_script: &ScriptBuf,
    total_output_count: usize,
    args: &PrepareArgs,
) -> Result<Vec<BuiltResetBatch>> {
    let mut sized = Vec::with_capacity(selected.len());
    for output in selected.drain(..) {
        let psbt = build_reset_psbt(
            wallet,
            std::slice::from_ref(&output),
            destination_script,
            1,
            args.fee_rate_sat_vb,
        )?;
        let signer_request_bytes = signer_request_body(&psbt, &args.signer_network)?.len();
        let conservative_weight_wu = conservative_signed_p2wpkh_weight(&psbt.unsigned_tx);
        let fee_sats = psbt.fee()?.to_sat();
        ensure!(
            signer_request_bytes <= args.max_signer_request_bytes,
            "outpoint {} alone needs a {}-byte signer request, exceeding the {}-byte cap",
            output.outpoint,
            signer_request_bytes,
            args.max_signer_request_bytes
        );
        ensure!(
            conservative_weight_wu <= args.max_weight_wu
                && conservative_weight_wu <= MAX_STANDARD_WEIGHT_WU,
            "outpoint {} alone exceeds a signed-weight cap",
            output.outpoint
        );
        ensure!(
            fee_sats <= args.max_fee_sats_per_batch,
            "outpoint {} alone exceeds the per-batch fee cap",
            output.outpoint
        );
        sized.push((signer_request_bytes, output));
    }
    sized.sort_by(|(left_size, left), (right_size, right)| {
        right_size
            .cmp(left_size)
            .then_with(|| left.outpoint.cmp(&right.outpoint))
    });
    selected.extend(sized.into_iter().map(|(_, output)| output));

    let input_count = selected.len();
    let minimum_batches = input_count
        .div_ceil(args.max_inputs)
        .max(total_output_count.div_ceil(args.max_outputs));
    let maximum_batches = input_count.min(total_output_count);
    ensure!(
        minimum_batches <= maximum_batches,
        "wallet reset cannot satisfy both the input and output caps"
    );

    for target_batches in minimum_batches..=maximum_batches {
        let output_counts =
            distribute_reset_outputs(total_output_count, target_batches, args.max_outputs)?;
        let mut start = 0_usize;
        let mut batches = Vec::with_capacity(target_batches);
        let mut target_is_feasible = true;
        for (batch_index, output_count) in output_counts.into_iter().enumerate() {
            let batches_after = target_batches - batch_index - 1;
            let max_end = (start + args.max_inputs).min(input_count - batches_after);
            let mut best = None;
            for end in (start + 1)..=max_end {
                let psbt = match build_reset_psbt(
                    wallet,
                    &selected[start..end],
                    destination_script,
                    output_count,
                    args.fee_rate_sat_vb,
                ) {
                    Ok(psbt) => psbt,
                    Err(_) => continue,
                };
                let Some((signer_request_bytes, conservative_weight_wu)) = batch_fits_caps(
                    &psbt,
                    &args.signer_network,
                    args.max_fee_sats_per_batch,
                    args.max_weight_wu,
                    args.max_signer_request_bytes,
                )?
                else {
                    break;
                };
                best = Some(BuiltResetBatch {
                    input_range: start..end,
                    psbt,
                    signer_request_bytes,
                    conservative_weight_wu,
                });
            }
            let Some(batch) = best else {
                target_is_feasible = false;
                break;
            };
            start = batch.input_range.end;
            batches.push(batch);
        }
        if target_is_feasible && start == input_count {
            let batch_values = batches
                .iter()
                .map(|batch| {
                    selected[batch.input_range.clone()]
                        .iter()
                        .try_fold(0_u64, |sum, output| {
                            sum.checked_add(output.txout.value.to_sat())
                                .context("wallet-reset batch value overflow")
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            let proportional_counts = distribute_reset_outputs_by_value(
                total_output_count,
                &batch_values,
                args.max_outputs,
            )?;
            let mut rebuilt = Vec::with_capacity(batches.len());
            let mut proportional_is_feasible = true;
            for (batch, output_count) in batches.into_iter().zip(proportional_counts) {
                let psbt = match build_reset_psbt(
                    wallet,
                    &selected[batch.input_range.clone()],
                    destination_script,
                    output_count,
                    args.fee_rate_sat_vb,
                ) {
                    Ok(psbt) => psbt,
                    Err(_) => {
                        proportional_is_feasible = false;
                        break;
                    }
                };
                let Some((signer_request_bytes, conservative_weight_wu)) = batch_fits_caps(
                    &psbt,
                    &args.signer_network,
                    args.max_fee_sats_per_batch,
                    args.max_weight_wu,
                    args.max_signer_request_bytes,
                )?
                else {
                    proportional_is_feasible = false;
                    break;
                };
                rebuilt.push(BuiltResetBatch {
                    input_range: batch.input_range,
                    psbt,
                    signer_request_bytes,
                    conservative_weight_wu,
                });
            }
            if proportional_is_feasible {
                return Ok(rebuilt);
            }
        }
    }
    bail!(
        "wallet-reset input set cannot fit the configured signer-request, input, output, fee, and weight caps"
    )
}

fn checked_input_records_total(inputs: &[InputRecord]) -> Result<u64> {
    inputs.iter().try_fold(0_u64, |sum, input| {
        sum.checked_add(input.value_sats)
            .context("input record value overflow")
    })
}

fn checked_output_records_total(outputs: &[OutputRecord]) -> Result<u64> {
    outputs.iter().try_fold(0_u64, |sum, output| {
        sum.checked_add(output.value_sats)
            .context("output record value overflow")
    })
}

fn reserved_inputs_digest<'a>(outpoints: impl IntoIterator<Item = &'a str>) -> Result<String> {
    let mut outpoints = outpoints
        .into_iter()
        .map(|value| OutPoint::from_str(value).map(|outpoint| outpoint.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    outpoints.sort_unstable();
    ensure!(
        outpoints.windows(2).all(|window| window[0] != window[1]),
        "reserved input set contains a duplicate outpoint"
    );
    let canonical = if outpoints.is_empty() {
        String::new()
    } else {
        format!("{}\n", outpoints.join("\n"))
    };
    Ok(sha256::Hash::hash(canonical.as_bytes()).to_string())
}

fn validate_batch_set_plan(plan: &BatchSetPlan) -> Result<()> {
    ensure!(
        plan.version == BATCH_SET_VERSION,
        "unsupported batch-set plan version"
    );
    ensure!(
        plan.plan_digest == plan.computed_digest()?,
        "batch-set plan digest is inconsistent"
    );
    ensure!(
        plan.maintenance_mode == MaintenanceMode::WalletReset
            && plan.require_drain_all
            && plan.known_utxo_sync_performed
            && plan.revealed_script_sync_performed
            && plan.preserved_input_count == 0,
        "batch-set plan is not an exhaustive zero-reserve wallet reset"
    );
    validate_descriptor_identity(&plan.source_descriptor_identity, false)?;
    validate_descriptor_identity(&plan.destination_descriptor_identity, true)?;
    ensure!(
        plan.source_descriptor_identity.descriptor_sha256
            != plan.destination_descriptor_identity.descriptor_sha256,
        "batch-set source and destination descriptors are identical"
    );
    ensure!(
        plan.xpub == plan.source_descriptor_identity.account_xpub
            && plan.master_fingerprint == plan.source_descriptor_identity.master_fingerprint,
        "batch-set source identity fields are inconsistent"
    );
    ensure!(
        plan.destination_keychain == "internal",
        "batch-set destination is not on the fresh internal keychain"
    );
    ensure!(
        plan.signer_network == "mutinynet"
            && plan.requested_fee_rate_sat_vb > 0
            && plan.max_inputs_per_batch > 0
            && plan.max_outputs_per_batch > 0
            && plan.max_total_fee_sats > 0
            && plan.max_fee_sats_per_batch > 0
            && plan.max_weight_wu_per_batch > 0
            && plan.max_weight_wu_per_batch <= MAX_STANDARD_WEIGHT_WU,
        "batch-set network or safety caps are invalid"
    );
    let destination = derive_bip84_destination(
        &plan.destination_descriptor_identity,
        KeychainKind::Internal,
        plan.destination_index,
        true,
    )?;
    ensure!(
        destination.to_string() == plan.destination,
        "batch-set destination does not match the fresh descriptor"
    );
    ensure!(
        (1..=ABSOLUTE_MAX_SIGNER_REQUEST_BYTES).contains(&plan.max_signer_request_bytes),
        "batch-set signer request cap exceeds the original signer limit"
    );
    ensure!(
        plan.batches.len() == plan.unsigned_txids.len() && !plan.batches.is_empty(),
        "batch-set transaction inventory is inconsistent"
    );

    let destination_script_hex = destination.script_pubkey().as_bytes().to_lower_hex_string();
    let mut seen_inputs = HashSet::new();
    let mut seen_txids = HashSet::new();
    let mut total_inputs = 0_u64;
    let mut total_outputs = 0_u64;
    let mut total_fees = 0_u64;
    let mut total_output_count = 0_usize;
    for (offset, batch) in plan.batches.iter().enumerate() {
        ensure!(
            batch.batch_index == offset + 1,
            "batch-set indices are not contiguous and one-based"
        );
        ensure!(
            batch.inputs.len() <= plan.max_inputs_per_batch
                && batch.outputs.len() <= plan.max_outputs_per_batch
                && !batch.inputs.is_empty()
                && !batch.outputs.is_empty(),
            "batch {} violates an input or output count cap",
            batch.batch_index
        );
        let psbt = Psbt::from_str(&batch.psbt)
            .with_context(|| format!("batch {} contains an invalid PSBT", batch.batch_index))?;
        let unsigned_txid = psbt.unsigned_tx.compute_txid().to_string();
        ensure!(
            unsigned_txid == batch.unsigned_txid
                && plan.unsigned_txids[offset] == batch.unsigned_txid
                && seen_txids.insert(unsigned_txid),
            "batch {} txid inventory is inconsistent",
            batch.batch_index
        );
        ensure!(
            psbt.unsigned_tx.input.len() == batch.inputs.len()
                && psbt.unsigned_tx.output.len() == batch.outputs.len()
                && batch.planned_output_count == batch.outputs.len(),
            "batch {} transaction counts are inconsistent",
            batch.batch_index
        );
        for (input_index, (txin, record)) in
            psbt.unsigned_tx.input.iter().zip(&batch.inputs).enumerate()
        {
            let outpoint = OutPoint::from_str(&record.outpoint)?;
            let expected_prevout = TxOut {
                value: Amount::from_sat(record.value_sats),
                script_pubkey: ScriptBuf::from_hex(&record.script_pubkey_hex)?,
            };
            let psbt_input = &psbt.inputs[input_index];
            let witness_utxo = psbt_input
                .witness_utxo
                .as_ref()
                .context("batch PSBT input has no witness_utxo")?;
            let parent = psbt_input
                .non_witness_utxo
                .as_ref()
                .context("batch PSBT input has no non_witness_utxo")?;
            let parent_output = parent
                .output
                .get(usize::try_from(outpoint.vout)?)
                .context("batch PSBT parent has no referenced output")?;
            ensure!(
                txin.previous_output == outpoint
                    && parent.compute_txid() == outpoint.txid
                    && witness_utxo == &expected_prevout
                    && parent_output == &expected_prevout
                    && seen_inputs.insert(outpoint),
                "batch-set input union is duplicated or inconsistent at {outpoint}"
            );
        }
        for (txout, record) in psbt.unsigned_tx.output.iter().zip(&batch.outputs) {
            ensure!(
                txout.value.to_sat() == record.value_sats
                    && txout.script_pubkey.as_bytes().to_lower_hex_string()
                        == record.script_pubkey_hex
                    && record.script_pubkey_hex == destination_script_hex,
                "batch {} contains an inconsistent destination output",
                batch.batch_index
            );
        }
        let input_total = checked_input_records_total(&batch.inputs)?;
        let output_total = checked_output_records_total(&batch.outputs)?;
        let fee_sats = input_total
            .checked_sub(output_total)
            .context("batch outputs exceed its recorded inputs")?;
        let request_bytes = signer_request_body(&psbt, &plan.signer_network)?.len();
        let conservative_weight_wu = conservative_signed_p2wpkh_weight(&psbt.unsigned_tx);
        ensure!(
            input_total == batch.input_total_sats
                && output_total == batch.output_total_sats
                && fee_sats == batch.fee_sats
                && fee_sats <= plan.max_fee_sats_per_batch
                && request_bytes == batch.signer_request_bytes
                && request_bytes <= plan.max_signer_request_bytes
                && conservative_weight_wu == batch.conservative_weight_wu
                && conservative_weight_wu <= plan.max_weight_wu_per_batch
                && conservative_weight_wu <= MAX_STANDARD_WEIGHT_WU,
            "batch {} totals or safety bounds are inconsistent",
            batch.batch_index
        );
        total_inputs = total_inputs
            .checked_add(input_total)
            .context("set input overflow")?;
        total_outputs = total_outputs
            .checked_add(output_total)
            .context("set output overflow")?;
        total_fees = total_fees
            .checked_add(fee_sats)
            .context("set fee overflow")?;
        total_output_count = total_output_count
            .checked_add(batch.outputs.len())
            .context("set output count overflow")?;
    }
    let accounted_inputs = seen_inputs
        .len()
        .checked_add(plan.excluded_eligible_input_count)
        .context("batch-set input accounting overflow")?;
    ensure!(
        accounted_inputs == plan.eligible_input_count,
        "batch-set input union omits or adds eligible source outpoints"
    );
    ensure!(
        total_inputs == plan.total_input_sats
            && total_outputs == plan.total_output_sats
            && total_fees == plan.total_fee_sats
            && total_fees <= plan.max_total_fee_sats
            && total_output_count == plan.planned_output_count,
        "batch-set aggregate totals are inconsistent"
    );
    Ok(())
}

fn batch_artifact_file(batch: &UnsignedBatchPlan) -> String {
    format!(
        "batch-{:03}-{}.json",
        batch.batch_index, batch.unsigned_txid
    )
}

fn batch_manifest_path(artifact_dir: &Path, plan_digest: &str) -> PathBuf {
    artifact_dir.join(format!("batch-set-{plan_digest}.manifest.json"))
}

fn maintenance_artifact_digest(artifact: &MaintenanceArtifact) -> Result<String> {
    Ok(sha256::Hash::hash(&serde_json::to_vec(artifact)?).to_string())
}

fn persist_batch_manifest(path: &Path, manifest: &BatchSetManifest, create: bool) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    if create {
        write_create_only(path, &bytes)
    } else {
        write_replace_private(path, &bytes)
    }
}

fn materialize_signed_records(
    artifact_dir: &Path,
    manifest: &mut BatchSetManifest,
) -> Result<bool> {
    let mut changed = false;
    for record in &mut manifest.signed_artifacts {
        changed |= materialize_signed_record(artifact_dir, record)?;
    }
    Ok(changed)
}

fn materialize_signed_record(artifact_dir: &Path, record: &mut SignedBatchRecord) -> Result<bool> {
    let path = artifact_dir.join(&record.artifact_file);
    let expected_digest = maintenance_artifact_digest(&record.artifact)?;
    ensure!(
        expected_digest == record.artifact_sha256,
        "signed batch {} has an inconsistent embedded artifact digest",
        record.batch_index
    );
    if path.exists() {
        let on_disk: MaintenanceArtifact = serde_json::from_slice(&fs::read(&path)?)?;
        ensure!(
            on_disk == record.artifact,
            "signed batch {} artifact file differs from the reserved manifest artifact",
            record.batch_index
        );
    } else {
        write_create_only(&path, &serde_json::to_vec_pretty(&record.artifact)?)?;
    }
    let changed = !record.materialized;
    record.materialized = true;
    Ok(changed)
}

fn validate_signed_manifest_records(manifest: &BatchSetManifest) -> Result<()> {
    ensure!(
        manifest.signed_artifacts.len() <= manifest.plan.batches.len(),
        "batch manifest has more signed records than planned batches"
    );
    let mut seen = HashSet::new();
    for record in &manifest.signed_artifacts {
        ensure!(
            seen.insert(record.batch_index),
            "batch manifest contains duplicate signed batch {}",
            record.batch_index
        );
        let batch = manifest
            .plan
            .batches
            .get(record.batch_index.saturating_sub(1))
            .context("signed batch record index is out of range")?;
        ensure!(
            batch.batch_index == record.batch_index
                && record.txid == batch.unsigned_txid
                && record.artifact_file == batch_artifact_file(batch)
                && record.artifact_sha256 == maintenance_artifact_digest(&record.artifact)?,
            "signed batch {} record differs from its plan",
            record.batch_index
        );
        validate_batch_artifact_against_plan(&record.artifact, &manifest.plan, batch)?;
    }
    match manifest.status {
        BatchSetStatus::Signing => {
            ensure!(
                manifest.signed_artifacts.is_empty(),
                "signing manifest already contains signed artifacts"
            );
        }
        BatchSetStatus::PartiallySigned => {
            ensure!(
                !manifest.signed_artifacts.is_empty()
                    && manifest.signed_artifacts.len() < manifest.plan.batches.len(),
                "partially-signed manifest has an invalid signed artifact count"
            );
        }
        BatchSetStatus::FullySigned => {
            ensure!(
                manifest.signed_artifacts.len() == manifest.plan.batches.len(),
                "fully-signed manifest does not contain every planned artifact"
            );
        }
    }
    Ok(())
}

fn initialize_or_resume_batch_manifest(
    artifact_dir: &Path,
    plan: &BatchSetPlan,
) -> Result<(PathBuf, BatchSetManifest)> {
    let path = batch_manifest_path(artifact_dir, &plan.plan_digest);
    let reserved_input_count = plan.batches.iter().map(|batch| batch.inputs.len()).sum();
    let reserved_inputs_sha256 = reserved_inputs_digest(
        plan.batches
            .iter()
            .flat_map(|batch| batch.inputs.iter().map(|input| input.outpoint.as_str())),
    )?;
    let manifest_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("batch manifest has no valid filename")?
        .to_owned();
    let mut allowed_names = plan
        .batches
        .iter()
        .map(batch_artifact_file)
        .collect::<HashSet<_>>();
    allowed_names.insert(manifest_name);
    if path.exists() {
        for entry in fs::read_dir(artifact_dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("artifact directory contains a non-UTF-8 name"))?;
            ensure!(
                allowed_names.contains(&name),
                "artifact directory contains stale or foreign batch-set state: {name}"
            );
        }
        let mut manifest: BatchSetManifest = serde_json::from_slice(&fs::read(&path)?)?;
        ensure!(
            manifest.version == BATCH_SET_VERSION
                && manifest.plan_digest == plan.plan_digest
                && manifest.plan == *plan
                && manifest.reserved_input_count == reserved_input_count
                && manifest.reserved_inputs_sha256 == reserved_inputs_sha256,
            "existing batch-set manifest does not reserve this exact approved plan"
        );
        validate_signed_manifest_records(&manifest)?;
        if materialize_signed_records(artifact_dir, &mut manifest)? {
            persist_batch_manifest(&path, &manifest, false)?;
        }
        return Ok((path, manifest));
    }

    ensure!(
        fs::read_dir(artifact_dir)?.next().is_none(),
        "a new wallet-reset signing run requires an empty artifact directory"
    );
    for batch in &plan.batches {
        ensure!(
            !artifact_dir.join(batch_artifact_file(batch)).exists(),
            "wallet-reset artifact exists without its exact global manifest"
        );
    }
    let manifest = BatchSetManifest {
        version: BATCH_SET_VERSION,
        plan_digest: plan.plan_digest.clone(),
        status: BatchSetStatus::Signing,
        reserved_input_count,
        reserved_inputs_sha256,
        plan: plan.clone(),
        signed_artifacts: Vec::new(),
        last_error: None,
    };
    persist_batch_manifest(&path, &manifest, true)?;
    Ok((path, manifest))
}

fn artifact_for_signed_batch(
    plan: &BatchSetPlan,
    batch: &UnsignedBatchPlan,
    signed_tx: &Transaction,
    snapshot_tip_hash: String,
) -> Result<MaintenanceArtifact> {
    let psbt = Psbt::from_str(&batch.psbt)?;
    let prevouts = input_records_prevouts(&batch.inputs)?;
    verify_signed_transaction_with_prevouts(&psbt, signed_tx, prevouts.clone())?;
    let fee_sats = transaction_fee_with_prevouts(signed_tx, &prevouts)?;
    ensure!(fee_sats == batch.fee_sats, "signed batch fee changed");
    let weight_wu = signed_tx.weight().to_wu();
    ensure!(
        weight_wu <= batch.conservative_weight_wu
            && weight_wu <= plan.max_weight_wu_per_batch
            && weight_wu <= MAX_STANDARD_WEIGHT_WU,
        "signed batch exceeds a weight cap"
    );
    let destination_script = Address::<NetworkUnchecked>::from_str(&plan.destination)?
        .require_network(Network::Signet)?
        .script_pubkey();
    Ok(MaintenanceArtifact {
        version: BATCH_SET_VERSION,
        created_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        snapshot_tip_height: plan.snapshot_tip_height,
        snapshot_tip_hash,
        remote_tip_height: plan.remote_tip_height,
        known_utxo_sync_performed: plan.known_utxo_sync_performed,
        revealed_script_sync_performed: plan.revealed_script_sync_performed,
        bridge_control_verified: false,
        maintenance_mode: MaintenanceMode::WalletReset,
        source_descriptor_identity: Some(plan.source_descriptor_identity.clone()),
        destination_descriptor_identity: Some(plan.destination_descriptor_identity.clone()),
        require_drain_all: true,
        eligible_input_count: plan.eligible_input_count,
        excluded_eligible_input_count: plan.excluded_eligible_input_count,
        preserved_input_count: 0,
        planned_output_count: batch.outputs.len(),
        xpub: plan.xpub.clone(),
        master_fingerprint: plan.master_fingerprint.clone(),
        signer_network: plan.signer_network.clone(),
        destination: plan.destination.clone(),
        destination_script_hex: destination_script.as_bytes().to_lower_hex_string(),
        destination_keychain: plan.destination_keychain.clone(),
        destination_index: plan.destination_index,
        exclusion_count: plan.exclusion_count,
        exclusion_sha256: plan.exclusion_sha256.clone(),
        inputs: batch.inputs.clone(),
        outputs: batch.outputs.clone(),
        psbt: batch.psbt.clone(),
        signed_tx_hex: serialize(signed_tx).to_lower_hex_string(),
        txid: signed_tx.compute_txid().to_string(),
        fee_sats,
        fee_rate_sat_vb: fee_sats as f64 / signed_tx.vsize() as f64,
        weight_wu,
        conservative_weight_wu: batch.conservative_weight_wu,
        max_fee_sats: plan.max_fee_sats_per_batch,
        max_weight_wu: plan.max_weight_wu_per_batch,
        batch_plan_digest: Some(plan.plan_digest.clone()),
        batch_index: Some(batch.batch_index),
        batch_count: Some(plan.batches.len()),
        signer_request_bytes: Some(batch.signer_request_bytes),
        max_signer_request_bytes: Some(plan.max_signer_request_bytes),
    })
}

fn input_records_prevouts(inputs: &[InputRecord]) -> Result<HashMap<OutPoint, TxOut>> {
    let mut prevouts = HashMap::new();
    for input in inputs {
        let outpoint = OutPoint::from_str(&input.outpoint)?;
        ensure!(
            prevouts
                .insert(
                    outpoint,
                    TxOut {
                        value: Amount::from_sat(input.value_sats),
                        script_pubkey: ScriptBuf::from_hex(&input.script_pubkey_hex)?,
                    },
                )
                .is_none(),
            "input records contain duplicate outpoint {outpoint}"
        );
    }
    Ok(prevouts)
}

#[allow(clippy::too_many_arguments)]
async fn prepare_wallet_reset_batch_set(
    args: &PrepareArgs,
    wallet: &mut ProviderWallet,
    client: &esplora_client::AsyncClient,
    snapshot_tip: u32,
    remote_tip: u32,
    source_identity: DescriptorIdentity,
    destination_identity: DescriptorIdentity,
    destination: Address,
    destination_keychain: String,
    destination_index: u32,
    destination_script: ScriptBuf,
    eligible_input_count: usize,
    excluded_eligible_input_count: usize,
    preserved_input_count: usize,
    exclusion_count: usize,
    exclusion_sha256: String,
    mut selected: Vec<LocalOutput>,
) -> Result<()> {
    let reset_output_count = args
        .reset_output_count
        .context("missing reset output count")?;
    let eligible_outpoints = selected
        .iter()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    ensure!(
        eligible_outpoints.len() + excluded_eligible_input_count == eligible_input_count,
        "wallet-reset selected union does not account for every eligible outpoint"
    );
    let built_batches = build_reset_batches(
        wallet,
        &mut selected,
        &destination_script,
        reset_output_count,
        args,
    )?;
    let mut batches = Vec::with_capacity(built_batches.len());
    for (offset, built) in built_batches.iter().enumerate() {
        let inputs = selected[built.input_range.clone()]
            .iter()
            .map(input_record)
            .collect::<Result<Vec<_>>>()?;
        let outputs = built
            .psbt
            .unsigned_tx
            .output
            .iter()
            .map(|output| OutputRecord {
                value_sats: output.value.to_sat(),
                script_pubkey_hex: output.script_pubkey.as_bytes().to_lower_hex_string(),
            })
            .collect::<Vec<_>>();
        let input_total_sats = checked_input_records_total(&inputs)?;
        let output_total_sats = checked_output_records_total(&outputs)?;
        let fee_sats = input_total_sats
            .checked_sub(output_total_sats)
            .context("wallet-reset batch outputs exceed inputs")?;
        batches.push(UnsignedBatchPlan {
            batch_index: offset + 1,
            unsigned_txid: built.psbt.unsigned_tx.compute_txid().to_string(),
            input_total_sats,
            output_total_sats,
            fee_sats,
            planned_output_count: outputs.len(),
            signer_request_bytes: built.signer_request_bytes,
            conservative_weight_wu: built.conservative_weight_wu,
            inputs,
            outputs,
            psbt: built.psbt.to_string(),
        });
    }
    let total_input_sats = batches.iter().try_fold(0_u64, |sum, batch| {
        sum.checked_add(batch.input_total_sats)
            .context("wallet-reset set input overflow")
    })?;
    let total_output_sats = batches.iter().try_fold(0_u64, |sum, batch| {
        sum.checked_add(batch.output_total_sats)
            .context("wallet-reset set output overflow")
    })?;
    let total_fee_sats = batches.iter().try_fold(0_u64, |sum, batch| {
        sum.checked_add(batch.fee_sats)
            .context("wallet-reset set fee overflow")
    })?;
    ensure!(
        total_fee_sats <= args.max_fee_sats,
        "wallet-reset batch set fee {total_fee_sats} exceeds the aggregate {}-sat cap",
        args.max_fee_sats
    );
    let unsigned_txids = batches
        .iter()
        .map(|batch| batch.unsigned_txid.clone())
        .collect::<Vec<_>>();
    let plan = BatchSetPlan {
        version: BATCH_SET_VERSION,
        plan_digest: String::new(),
        snapshot_tip_height: snapshot_tip,
        remote_tip_height: remote_tip,
        known_utxo_sync_performed: true,
        revealed_script_sync_performed: true,
        maintenance_mode: MaintenanceMode::WalletReset,
        source_descriptor_identity: source_identity,
        destination_descriptor_identity: destination_identity,
        require_drain_all: true,
        eligible_input_count,
        excluded_eligible_input_count,
        preserved_input_count,
        planned_output_count: reset_output_count,
        destination: destination.to_string(),
        destination_keychain,
        destination_index,
        xpub: args.wallet.xpub.to_string(),
        master_fingerprint: args.wallet.master_fingerprint.to_string(),
        signer_network: args.signer_network.clone(),
        exclusion_count,
        exclusion_sha256,
        total_input_sats,
        total_output_sats,
        total_fee_sats,
        requested_fee_rate_sat_vb: args.fee_rate_sat_vb,
        max_inputs_per_batch: args.max_inputs,
        max_outputs_per_batch: args.max_outputs,
        max_total_fee_sats: args.max_fee_sats,
        max_fee_sats_per_batch: args.max_fee_sats_per_batch,
        max_weight_wu_per_batch: args.max_weight_wu,
        max_signer_request_bytes: args.max_signer_request_bytes,
        unsigned_txids,
        batches,
    }
    .seal()?;
    validate_batch_set_plan(&plan)?;

    if args.dry_run {
        let bytes = serde_json::to_vec_pretty(&plan)?;
        write_create_only(
            args.plan_output.as_deref().context("missing plan output")?,
            &bytes,
        )?;
        println!("{}", String::from_utf8(bytes)?);
        return Ok(());
    }

    let approved_path = args
        .approved_plan
        .as_deref()
        .context("missing approved plan")?;
    let approved: BatchSetPlan = serde_json::from_slice(&fs::read(approved_path)?)?;
    validate_batch_set_plan(&approved)?;
    ensure!(
        approved.signing_commitment() == plan.signing_commitment(),
        "approved wallet-reset batch set does not match the exact rebuilt set"
    );
    ensure!(
        args.confirm_batch_plan_digest.as_deref() == Some(approved.plan_digest.as_str()),
        "confirmed batch-set digest does not match the approved plan"
    );

    let all_outpoints = plan
        .batches
        .iter()
        .flat_map(|batch| batch.inputs.iter())
        .map(|input| OutPoint::from_str(&input.outpoint))
        .collect::<Result<Vec<_>, _>>()?;
    check_unspent(client, &all_outpoints).await?;
    let (manifest_path, mut manifest) =
        initialize_or_resume_batch_manifest(&args.artifact_dir, &approved)?;
    ensure!(
        manifest.status != BatchSetStatus::FullySigned
            || manifest.signed_artifacts.len() == approved.batches.len(),
        "fully-signed manifest has an incomplete artifact inventory"
    );

    for batch in &approved.batches {
        if manifest
            .signed_artifacts
            .iter()
            .any(|record| record.batch_index == batch.batch_index)
        {
            continue;
        }
        let batch_outpoints = batch
            .inputs
            .iter()
            .map(|input| OutPoint::from_str(&input.outpoint))
            .collect::<Result<Vec<_>, _>>()?;
        check_unspent(client, &batch_outpoints).await?;
        let psbt = Psbt::from_str(&batch.psbt)?;
        let signed = match remote_sign(
            &psbt,
            &approved.signer_network,
            args.signer_url.as_deref().context("missing signer URL")?,
            args.signer_auth_key
                .as_deref()
                .context("missing signer auth key")?,
            approved.max_signer_request_bytes,
        )
        .await
        {
            Ok(signed) => signed,
            Err(error) => {
                manifest.status = if manifest.signed_artifacts.is_empty() {
                    BatchSetStatus::Signing
                } else {
                    BatchSetStatus::PartiallySigned
                };
                manifest.last_error = Some(format!(
                    "batch {} signing failed: {error:#}",
                    batch.batch_index
                ));
                persist_batch_manifest(&manifest_path, &manifest, false)?;
                return Err(error).with_context(|| format!("signing batch {}", batch.batch_index));
            }
        };
        let artifact = artifact_for_signed_batch(
            &approved,
            batch,
            &signed,
            wallet.latest_checkpoint().hash().to_string(),
        )?;
        let artifact_file = batch_artifact_file(batch);
        let artifact_sha256 = maintenance_artifact_digest(&artifact)?;
        manifest.signed_artifacts.push(SignedBatchRecord {
            batch_index: batch.batch_index,
            txid: artifact.txid.clone(),
            artifact_file: artifact_file.clone(),
            artifact_sha256,
            materialized: false,
            artifact,
        });
        manifest
            .signed_artifacts
            .sort_by_key(|record| record.batch_index);
        manifest.status = if manifest.signed_artifacts.len() == approved.batches.len() {
            BatchSetStatus::FullySigned
        } else {
            BatchSetStatus::PartiallySigned
        };
        manifest.last_error = None;
        // Reserve the exact signed transaction durably inside the global manifest before
        // creating its separate artifact. Resume can materialize this record without signing again.
        persist_batch_manifest(&manifest_path, &manifest, false)?;
        if materialize_signed_records(&args.artifact_dir, &mut manifest)? {
            persist_batch_manifest(&manifest_path, &manifest, false)?;
        }
    }
    ensure!(
        manifest.status == BatchSetStatus::FullySigned
            && manifest.signed_artifacts.len() == approved.batches.len(),
        "wallet-reset batch-set signing stopped before every transaction had an artifact"
    );
    // A complete live union check is the final gate before any artifact can be broadcast.
    check_unspent(client, &all_outpoints).await?;
    persist_batch_manifest(&manifest_path, &manifest, false)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    eprintln!("batch manifest: {}", manifest_path.display());
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

fn signer_request_body(psbt: &Psbt, network: &str) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&SignRequest {
        psbt: psbt.to_string(),
        network,
    })?)
}

async fn remote_sign(
    psbt: &Psbt,
    network: &str,
    signer_url: &str,
    auth_key_path: &Path,
    max_request_bytes: usize,
) -> Result<Transaction> {
    let pem = fs::read_to_string(auth_key_path)
        .with_context(|| format!("reading signer auth key {}", auth_key_path.display()))?;
    let key = SigningKey::from_pkcs8_pem(&pem).context("parsing signer auth key")?;
    let body = signer_request_body(psbt, network)?;
    ensure!(
        body.len() <= max_request_bytes,
        "exact serialized signer JSON request is {} bytes, exceeding the {}-byte cap",
        body.len(),
        max_request_bytes
    );
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

fn conservative_signed_p2wpkh_weight(unsigned_tx: &Transaction) -> u64 {
    let mut estimated = unsigned_tx.clone();
    for input in &mut estimated.input {
        input.witness = Witness::from_slice(&[vec![0_u8; 73], vec![0_u8; 33]]);
    }
    estimated.weight().to_wu()
}

fn parse_keychain(value: &str) -> Result<KeychainKind> {
    match value {
        "external" => Ok(KeychainKind::External),
        "internal" => Ok(KeychainKind::Internal),
        _ => bail!("invalid destination keychain: {value}"),
    }
}

fn validate_versioned_artifact(
    artifact: &MaintenanceArtifact,
    destination: &Address,
    destination_script: &ScriptBuf,
) -> Result<()> {
    if artifact.version == LEGACY_ARTIFACT_VERSION {
        return Ok(());
    }
    ensure!(
        artifact.version == ARTIFACT_VERSION,
        "unsupported artifact version"
    );
    let source_identity = artifact
        .source_descriptor_identity
        .as_ref()
        .context("version 3 artifact has no source descriptor identity")?;
    validate_descriptor_identity(source_identity, false)?;
    ensure!(
        artifact.xpub == source_identity.account_xpub
            && artifact.master_fingerprint == source_identity.master_fingerprint,
        "artifact source wallet fields do not match the source descriptor identity"
    );
    ensure!(
        artifact.planned_output_count == artifact.outputs.len(),
        "artifact output count differs from its approved plan metadata"
    );
    match artifact.maintenance_mode {
        MaintenanceMode::Consolidate | MaintenanceMode::WalletReset => {
            let destination_identity = artifact
                .destination_descriptor_identity
                .as_ref()
                .context("descriptor-owned artifact has no destination descriptor identity")?;
            let require_fresh_account = artifact.maintenance_mode == MaintenanceMode::WalletReset;
            validate_descriptor_identity(destination_identity, require_fresh_account)?;
            let keychain = parse_keychain(&artifact.destination_keychain)?;
            let derived_destination = derive_bip84_destination(
                destination_identity,
                keychain,
                artifact.destination_index,
                require_fresh_account,
            )?;
            ensure!(
                derived_destination == *destination
                    && derived_destination.script_pubkey() == *destination_script,
                "artifact destination does not match its descriptor identity and derivation index"
            );
            ensure!(
                !artifact.bridge_control_verified,
                "descriptor-owned artifact contains bridge-control metadata"
            );
            if artifact.maintenance_mode == MaintenanceMode::WalletReset {
                ensure!(
                    artifact.known_utxo_sync_performed && artifact.revealed_script_sync_performed,
                    "wallet-reset artifact was not built from an exhaustive revealed-script sync"
                );
                ensure!(
                    artifact.require_drain_all,
                    "wallet-reset artifact does not require a full drain"
                );
                ensure!(
                    artifact.preserved_input_count == 0,
                    "wallet-reset artifact preserves source-wallet UTXOs"
                );
                ensure!(
                    keychain == KeychainKind::Internal,
                    "wallet-reset artifact does not use the fresh wallet internal keychain"
                );
                ensure!(
                    source_identity.descriptor_sha256 != destination_identity.descriptor_sha256,
                    "wallet-reset artifact has identical source and destination descriptors"
                );
                let accounted_inputs = artifact
                    .inputs
                    .len()
                    .checked_add(artifact.excluded_eligible_input_count)
                    .context("wallet-reset input accounting overflow")?;
                ensure!(
                    accounted_inputs == artifact.eligible_input_count,
                    "wallet-reset artifact omits or adds source-wallet inputs"
                );
            }
        }
        MaintenanceMode::Bridge => {
            ensure!(
                artifact.destination_descriptor_identity.is_none(),
                "bridge artifact must not claim a destination descriptor identity"
            );
            ensure!(
                artifact.destination_keychain == "bridge" && artifact.destination_index == 0,
                "bridge artifact has invalid external-destination metadata"
            );
            ensure!(
                artifact.bridge_control_verified,
                "bridge artifact does not record the bridge-control acknowledgement"
            );
            ensure!(
                artifact.known_utxo_sync_performed && artifact.revealed_script_sync_performed,
                "bridge artifact was not built from an exhaustive revealed-script sync"
            );
            ensure!(
                artifact.require_drain_all
                    && artifact.preserved_input_count == 0
                    && artifact.excluded_eligible_input_count == 0,
                "bridge artifact is not an exact zero-reserve drain"
            );
            ensure!(
                artifact.exclusion_count == 0
                    && artifact.exclusion_sha256 == exclusion_digest(&HashSet::new()),
                "bridge artifact does not contain the required empty exclusion manifest"
            );
            ensure!(
                artifact.inputs.len() == artifact.eligible_input_count,
                "bridge artifact omits or adds source-wallet inputs"
            );
            ensure!(
                artifact.planned_output_count == 1 && artifact.outputs.len() == 1,
                "bridge artifact must contain exactly one destination output"
            );
        }
    }
    Ok(())
}

fn validate_batch_artifact_against_plan(
    artifact: &MaintenanceArtifact,
    plan: &BatchSetPlan,
    batch: &UnsignedBatchPlan,
) -> Result<()> {
    ensure!(
        artifact.version == BATCH_SET_VERSION,
        "batch artifact is not version 4"
    );
    ensure!(
        artifact.maintenance_mode == MaintenanceMode::WalletReset
            && artifact.batch_plan_digest.as_deref() == Some(plan.plan_digest.as_str())
            && artifact.batch_index == Some(batch.batch_index)
            && artifact.batch_count == Some(plan.batches.len())
            && artifact.signer_request_bytes == Some(batch.signer_request_bytes)
            && artifact.max_signer_request_bytes == Some(plan.max_signer_request_bytes),
        "batch artifact is not bound to its exact global plan position"
    );
    ensure!(
        artifact.source_descriptor_identity.as_ref() == Some(&plan.source_descriptor_identity)
            && artifact.destination_descriptor_identity.as_ref()
                == Some(&plan.destination_descriptor_identity)
            && artifact.require_drain_all
            && artifact.eligible_input_count == plan.eligible_input_count
            && artifact.excluded_eligible_input_count == plan.excluded_eligible_input_count
            && artifact.preserved_input_count == 0,
        "batch artifact global wallet-reset metadata is inconsistent"
    );
    ensure!(
        artifact.xpub == plan.xpub
            && artifact.master_fingerprint == plan.master_fingerprint
            && artifact.signer_network == plan.signer_network
            && artifact.destination == plan.destination
            && artifact.destination_keychain == plan.destination_keychain
            && artifact.destination_index == plan.destination_index
            && artifact.exclusion_count == plan.exclusion_count
            && artifact.exclusion_sha256 == plan.exclusion_sha256,
        "batch artifact wallet or exclusion identity is inconsistent"
    );
    ensure!(
        artifact.inputs == batch.inputs
            && artifact.outputs == batch.outputs
            && artifact.psbt == batch.psbt
            && artifact.txid == batch.unsigned_txid
            && artifact.fee_sats == batch.fee_sats
            && artifact.planned_output_count == batch.planned_output_count
            && artifact.conservative_weight_wu == batch.conservative_weight_wu
            && artifact.max_fee_sats == plan.max_fee_sats_per_batch
            && artifact.max_weight_wu == plan.max_weight_wu_per_batch,
        "batch artifact differs from its exact unsigned batch plan"
    );
    let signed: Transaction = deserialize(&hex::decode(&artifact.signed_tx_hex)?)?;
    let psbt = Psbt::from_str(&artifact.psbt)?;
    let prevouts = input_records_prevouts(&artifact.inputs)?;
    verify_signed_transaction_with_prevouts(&psbt, &signed, prevouts.clone())?;
    ensure!(
        signed.compute_txid().to_string() == batch.unsigned_txid
            && transaction_fee_with_prevouts(&signed, &prevouts)? == batch.fee_sats
            && signed.weight().to_wu() == artifact.weight_wu
            && artifact.weight_wu <= batch.conservative_weight_wu,
        "batch artifact signed transaction is inconsistent"
    );
    Ok(())
}

fn load_batch_manifest_for_artifact(
    path: &Path,
    artifact_path: &Path,
    artifact: &MaintenanceArtifact,
    confirmed_digest: Option<&str>,
) -> Result<BatchSetManifest> {
    let manifest: BatchSetManifest = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        manifest.version == BATCH_SET_VERSION
            && manifest.plan_digest == manifest.plan.plan_digest
            && confirmed_digest == Some(manifest.plan_digest.as_str()),
        "batch manifest version or confirmed plan digest is inconsistent"
    );
    validate_batch_set_plan(&manifest.plan)?;
    let reserved = manifest
        .plan
        .batches
        .iter()
        .flat_map(|batch| batch.inputs.iter().map(|input| input.outpoint.as_str()))
        .collect::<Vec<_>>();
    ensure!(
        manifest.reserved_input_count == reserved.len()
            && manifest.reserved_inputs_sha256 == reserved_inputs_digest(reserved.iter().copied())?,
        "batch manifest reserved input inventory is inconsistent"
    );
    ensure!(
        manifest.status == BatchSetStatus::FullySigned
            && manifest.signed_artifacts.len() == manifest.plan.batches.len(),
        "batch manifest is not fully signed; no artifact may be broadcast"
    );
    let mut seen_indices = HashSet::new();
    for record in &manifest.signed_artifacts {
        ensure!(
            record.materialized && seen_indices.insert(record.batch_index),
            "batch manifest has an unmaterialized or duplicate signed record"
        );
        let batch = manifest
            .plan
            .batches
            .get(record.batch_index.saturating_sub(1))
            .context("batch manifest signed record index is out of range")?;
        ensure!(
            batch.batch_index == record.batch_index
                && record.txid == batch.unsigned_txid
                && record.artifact_file == batch_artifact_file(batch)
                && record.artifact_sha256 == maintenance_artifact_digest(&record.artifact)?,
            "batch manifest signed record is inconsistent"
        );
        validate_batch_artifact_against_plan(&record.artifact, &manifest.plan, batch)?;
    }
    let artifact_index = artifact
        .batch_index
        .context("version 4 artifact has no batch index")?;
    let record = manifest
        .signed_artifacts
        .iter()
        .find(|record| record.batch_index == artifact_index)
        .context("artifact is not present in the fully-signed batch manifest")?;
    ensure!(
        record.artifact == *artifact
            && artifact_path.file_name().and_then(|name| name.to_str())
                == Some(record.artifact_file.as_str()),
        "artifact file does not match its exact global manifest record"
    );
    Ok(manifest)
}

async fn check_batch_manifest_outspends(
    client: &esplora_client::AsyncClient,
    manifest: &BatchSetManifest,
    current_batch_index: usize,
) -> Result<bool> {
    let expected = manifest
        .plan
        .batches
        .iter()
        .flat_map(|batch| {
            batch.inputs.iter().map(move |input| {
                Ok::<_, anyhow::Error>((
                    OutPoint::from_str(&input.outpoint)?,
                    Txid::from_str(&batch.unsigned_txid)?,
                    batch.batch_index,
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let statuses = stream::iter(expected)
        .map(|(outpoint, expected_spender, batch_index)| {
            let client = client.clone();
            async move {
                let status = client
                    .get_output_status(&outpoint.txid, u64::from(outpoint.vout))
                    .await?
                    .with_context(|| format!("Esplora does not know outpoint {outpoint}"))?;
                Ok::<_, anyhow::Error>((outpoint, expected_spender, batch_index, status))
            }
        })
        .buffer_unordered(16)
        .try_collect::<Vec<_>>()
        .await?;
    let mut current_spent = 0_usize;
    let mut current_total = 0_usize;
    for (outpoint, expected_spender, batch_index, status) in statuses {
        if batch_index == current_batch_index {
            current_total += 1;
        }
        if !status.spent {
            continue;
        }
        match status.txid {
            Some(spender) if spender == expected_spender => {
                if batch_index == current_batch_index {
                    current_spent += 1;
                }
            }
            Some(spender) => {
                bail!("reserved batch-set outpoint {outpoint} was spent by competing transaction {spender}")
            }
            None => {
                bail!("reserved batch-set outpoint {outpoint} is spent without a reported spender")
            }
        }
    }
    ensure!(
        current_spent == 0 || current_spent == current_total,
        "Esplora reports a partial exact spend for the selected batch"
    );
    Ok(current_spent == current_total)
}

async fn broadcast(args: BroadcastArgs) -> Result<()> {
    ensure!(
        args.confirm_safe_to_broadcast == BROADCAST_CONFIRMATION,
        "broadcast acknowledgement must be `{BROADCAST_CONFIRMATION}`"
    );
    let artifact: MaintenanceArtifact = serde_json::from_slice(&fs::read(&args.artifact)?)?;
    ensure!(
        (LEGACY_ARTIFACT_VERSION..=BATCH_SET_VERSION).contains(&artifact.version),
        "unsupported artifact version"
    );
    let batch_manifest = if artifact.version == BATCH_SET_VERSION {
        Some(load_batch_manifest_for_artifact(
            args.batch_manifest
                .as_deref()
                .context("version 4 batch artifact requires --batch-manifest")?,
            &args.artifact,
            &artifact,
            args.confirm_batch_plan_digest.as_deref(),
        )?)
    } else {
        ensure!(
            args.batch_manifest.is_none() && args.confirm_batch_plan_digest.is_none(),
            "batch manifest options are valid only for version 4 artifacts"
        );
        None
    };
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
    if artifact.version >= ARTIFACT_VERSION {
        let conservative_weight_wu = conservative_signed_p2wpkh_weight(&psbt.unsigned_tx);
        ensure!(
            artifact.conservative_weight_wu == conservative_weight_wu,
            "artifact conservative weight estimate is inconsistent"
        );
        ensure!(
            artifact.weight_wu <= conservative_weight_wu,
            "signed artifact exceeds its conservative P2WPKH weight estimate"
        );
        ensure!(
            conservative_weight_wu <= artifact.max_weight_wu
                && conservative_weight_wu <= MAX_STANDARD_WEIGHT_WU,
            "artifact conservative weight estimate exceeds a weight cap"
        );
    }
    let destination_script = ScriptBuf::from_hex(&artifact.destination_script_hex)?;
    let destination = Address::<NetworkUnchecked>::from_str(&artifact.destination)?
        .require_network(Network::Signet)?;
    ensure!(
        destination.script_pubkey() == destination_script,
        "artifact destination address and script are inconsistent"
    );
    if artifact.version <= ARTIFACT_VERSION {
        validate_versioned_artifact(&artifact, &destination, &destination_script)?;
    }
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
    if let Some(manifest) = &batch_manifest {
        let already_spent = check_batch_manifest_outspends(
            &client,
            manifest,
            artifact
                .batch_index
                .context("batch artifact has no index")?,
        )
        .await?;
        if already_spent {
            let status = client.get_tx_status(&artifact_txid).await?;
            println!(
                "{}",
                serde_json::json!({
                    "txid": artifact_txid,
                    "publication": "already_known",
                    "confirmed": status.confirmed,
                    "block_height": status.block_height,
                    "batch_plan_digest": manifest.plan_digest,
                })
            );
            return Ok(());
        }
    }
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

fn write_replace_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading manifest metadata: {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "refusing to replace a non-regular or symbolic-link manifest"
    );
    let parent = path.parent().context("manifest path has no parent")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid manifest filename")?;
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
        fs::rename(&temp_path, path)?;
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
    fn wallet_reset_requires_the_exact_non_excluded_set() {
        let first = OutPoint::new(Txid::from_byte_array([1; 32]), 0);
        let second = OutPoint::new(Txid::from_byte_array([2; 32]), 1);
        let third = OutPoint::new(Txid::from_byte_array([3; 32]), 2);
        let extra = OutPoint::new(Txid::from_byte_array([4; 32]), 3);
        let eligible = [first, second, third];
        let excluded = HashSet::from([second]);

        assert_eq!(
            verify_drain_all_outpoints(&eligible, &excluded, &[first, third]).unwrap(),
            1
        );
        assert!(verify_drain_all_outpoints(&eligible, &excluded, &[first]).is_err());
        assert!(verify_drain_all_outpoints(&eligible, &excluded, &[first, third, extra]).is_err());
        assert!(verify_drain_all_outpoints(&eligible, &excluded, &[first, second, third]).is_err());
    }

    #[test]
    fn fresh_bip84_destination_is_derived_from_the_supplied_identity() {
        let account_xpub = Xpub::from_str(
            "tpubDC2Qwo2TFsaNC4ju8nrUJ9mqVT3eSgdmy1yPqhgkjwmke3PRXutNGRYAUo6RCHTcVQaDR3ohNU9we59brGHuEKPvH1ags2nevW5opEE9Z5Q",
        )
        .unwrap();
        let fingerprint = Fingerprint::from_str("c55b303f").unwrap();
        let identity =
            bip84_descriptor_identity(account_xpub, fingerprint, Network::Signet, true).unwrap();
        let index = 7;
        let derived =
            derive_bip84_destination(&identity, KeychainKind::Internal, index, true).unwrap();

        let secp = Secp256k1::verification_only();
        let child = account_xpub
            .derive_pub(
                &secp,
                &[
                    ChildNumber::Normal { index: 1 },
                    ChildNumber::Normal { index },
                ],
            )
            .unwrap();
        let expected = Address::p2wpkh(&child.to_pub(), Network::Signet);
        assert_eq!(derived, expected);
        assert_eq!(identity.network, "signet");
        assert!(identity.internal_descriptor.contains("/1/*"));
        assert_eq!(identity.descriptor_sha256.len(), 64);
    }

    #[test]
    fn descriptor_identity_validation_fails_on_metadata_changes() {
        let account_xpub = Xpub::from_str(
            "tpubDC2Qwo2TFsaNC4ju8nrUJ9mqVT3eSgdmy1yPqhgkjwmke3PRXutNGRYAUo6RCHTcVQaDR3ohNU9we59brGHuEKPvH1ags2nevW5opEE9Z5Q",
        )
        .unwrap();
        let fingerprint = Fingerprint::from_str("c55b303f").unwrap();
        let mut identity =
            bip84_descriptor_identity(account_xpub, fingerprint, Network::Signet, true).unwrap();
        validate_descriptor_identity(&identity, true).unwrap();

        identity.internal_descriptor.push('0');
        assert!(validate_descriptor_identity(&identity, true).is_err());
    }

    fn unsigned_plan_fixture() -> UnsignedPlanReport {
        let account_xpub = Xpub::from_str(
            "tpubDC2Qwo2TFsaNC4ju8nrUJ9mqVT3eSgdmy1yPqhgkjwmke3PRXutNGRYAUo6RCHTcVQaDR3ohNU9we59brGHuEKPvH1ags2nevW5opEE9Z5Q",
        )
        .unwrap();
        let identity = bip84_descriptor_identity(
            account_xpub,
            Fingerprint::from_str("c55b303f").unwrap(),
            Network::Signet,
            true,
        )
        .unwrap();
        UnsignedPlanReport {
            version: ARTIFACT_VERSION,
            unsigned_txid: Txid::from_byte_array([9; 32]).to_string(),
            snapshot_tip_height: 100,
            remote_tip_height: 101,
            known_utxo_sync_performed: true,
            revealed_script_sync_performed: true,
            bridge_control_verified: false,
            maintenance_mode: MaintenanceMode::WalletReset,
            source_descriptor_identity: identity.clone(),
            destination_descriptor_identity: Some(identity.clone()),
            require_drain_all: true,
            eligible_input_count: 1,
            excluded_eligible_input_count: 0,
            preserved_input_count: 0,
            planned_output_count: 1,
            destination: "tb1qexample".to_string(),
            destination_keychain: "internal".to_string(),
            destination_index: 0,
            xpub: identity.account_xpub.clone(),
            master_fingerprint: identity.master_fingerprint.clone(),
            signer_network: "mutinynet".to_string(),
            exclusion_count: 0,
            exclusion_sha256: sha256::Hash::hash(b"").to_string(),
            inputs: vec![InputRecord {
                outpoint: OutPoint::new(Txid::from_byte_array([8; 32]), 0).to_string(),
                value_sats: 10_000,
                script_pubkey_hex: "0014".to_string(),
                confirmation_height: 90,
            }],
            outputs: vec![OutputRecord {
                value_sats: 9_000,
                script_pubkey_hex: "0014".to_string(),
            }],
            psbt: "approved-psbt".to_string(),
            fee_sats: 1_000,
            requested_fee_rate_sat_vb: 2,
            conservative_weight_wu: 1_000,
            max_fee_sats: 2_000,
            max_weight_wu: 200_000,
        }
    }

    #[test]
    fn signing_commitment_ignores_only_moving_tip_observations() {
        let approved = unsigned_plan_fixture();
        let mut rebuilt = approved.clone();
        rebuilt.snapshot_tip_height += 3;
        rebuilt.remote_tip_height += 4;
        assert_eq!(approved.signing_commitment(), rebuilt.signing_commitment());

        let mut changed = rebuilt.clone();
        changed.psbt.push('x');
        assert_ne!(approved.signing_commitment(), changed.signing_commitment());

        let mut changed = rebuilt.clone();
        changed.inputs[0].value_sats += 1;
        assert_ne!(approved.signing_commitment(), changed.signing_commitment());

        let mut changed = rebuilt.clone();
        changed.source_descriptor_identity.descriptor_sha256 = "0".repeat(64);
        assert_ne!(approved.signing_commitment(), changed.signing_commitment());

        let mut changed = rebuilt.clone();
        changed.require_drain_all = false;
        assert_ne!(approved.signing_commitment(), changed.signing_commitment());

        let mut changed = rebuilt;
        changed.maintenance_mode = MaintenanceMode::Bridge;
        assert_ne!(approved.signing_commitment(), changed.signing_commitment());
    }

    fn bridge_artifact_fixture() -> (MaintenanceArtifact, Address, ScriptBuf) {
        let source_xpub = Xpub::from_str(
            "tpubDC2Qwo2TFsaNC4ju8nrUJ9mqVT3eSgdmy1yPqhgkjwmke3PRXutNGRYAUo6RCHTcVQaDR3ohNU9we59brGHuEKPvH1ags2nevW5opEE9Z5Q",
        )
        .unwrap();
        let source_identity = bip84_descriptor_identity(
            source_xpub,
            Fingerprint::from_str("c55b303f").unwrap(),
            Network::Signet,
            false,
        )
        .unwrap();
        let secp = Secp256k1::new();
        let bridge_key = SecretKey::from_slice(&[99; 32]).unwrap();
        let bridge_public_key = bdk_wallet::bitcoin::PublicKey::new(
            bdk_wallet::bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &bridge_key),
        );
        let destination = Address::p2wpkh(&bridge_public_key.try_into().unwrap(), Network::Signet);
        let destination_script = destination.script_pubkey();
        let input = InputRecord {
            outpoint: OutPoint::new(Txid::from_byte_array([5; 32]), 0).to_string(),
            value_sats: 50_000,
            script_pubkey_hex: "0014".to_string(),
            confirmation_height: 100,
        };
        let output = OutputRecord {
            value_sats: 49_000,
            script_pubkey_hex: destination_script.as_bytes().to_lower_hex_string(),
        };
        let artifact = MaintenanceArtifact {
            version: ARTIFACT_VERSION,
            created_at_unix: 1,
            snapshot_tip_height: 101,
            snapshot_tip_hash: "00".repeat(32),
            remote_tip_height: 102,
            known_utxo_sync_performed: true,
            revealed_script_sync_performed: true,
            bridge_control_verified: true,
            maintenance_mode: MaintenanceMode::Bridge,
            source_descriptor_identity: Some(source_identity.clone()),
            destination_descriptor_identity: None,
            require_drain_all: true,
            eligible_input_count: 1,
            excluded_eligible_input_count: 0,
            preserved_input_count: 0,
            planned_output_count: 1,
            xpub: source_identity.account_xpub,
            master_fingerprint: source_identity.master_fingerprint,
            signer_network: "mutinynet".to_string(),
            destination: destination.to_string(),
            destination_script_hex: destination_script.as_bytes().to_lower_hex_string(),
            destination_keychain: "bridge".to_string(),
            destination_index: 0,
            exclusion_count: 0,
            exclusion_sha256: exclusion_digest(&HashSet::new()),
            inputs: vec![input],
            outputs: vec![output],
            psbt: String::new(),
            signed_tx_hex: String::new(),
            txid: Txid::from_byte_array([6; 32]).to_string(),
            fee_sats: 1_000,
            fee_rate_sat_vb: 1.0,
            weight_wu: 500,
            conservative_weight_wu: 600,
            max_fee_sats: 2_000,
            max_weight_wu: 1_000,
            batch_plan_digest: None,
            batch_index: None,
            batch_count: None,
            signer_request_bytes: None,
            max_signer_request_bytes: None,
        };
        (artifact, destination, destination_script)
    }

    #[test]
    fn bridge_artifact_requires_exact_full_drain_metadata() {
        let (artifact, destination, destination_script) = bridge_artifact_fixture();
        validate_versioned_artifact(&artifact, &destination, &destination_script).unwrap();

        let mut changed = artifact.clone();
        changed.bridge_control_verified = false;
        assert!(validate_versioned_artifact(&changed, &destination, &destination_script).is_err());

        let mut changed = artifact.clone();
        changed.exclusion_count = 1;
        assert!(validate_versioned_artifact(&changed, &destination, &destination_script).is_err());

        let mut changed = artifact.clone();
        changed.destination_descriptor_identity = changed.source_descriptor_identity.clone();
        assert!(validate_versioned_artifact(&changed, &destination, &destination_script).is_err());

        let mut changed = artifact;
        changed.outputs.push(changed.outputs[0].clone());
        assert!(validate_versioned_artifact(&changed, &destination, &destination_script).is_err());
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

    #[test]
    fn value_proportional_batch_outputs_are_exact_and_deterministic() {
        let values = [10_u64, 20, 70];
        let first = distribute_reset_outputs_by_value(20, &values, 10).unwrap();
        let second = distribute_reset_outputs_by_value(20, &values, 10).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.iter().sum::<usize>(), 20);
        assert!(first.iter().all(|count| (1..=10).contains(count)));
        assert!(first[2] > first[1] && first[1] >= first[0]);
    }

    fn repeated_parent_psbt(padding_bytes: usize, spent_outputs: usize) -> (Transaction, Psbt) {
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[77; 32]).unwrap();
        let public_key = bdk_wallet::bitcoin::PublicKey::new(
            bdk_wallet::bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &secret_key),
        );
        let script_pubkey = ScriptBuf::new_p2wpkh(&public_key.wpubkey_hash().unwrap());
        let mut parent_outputs = (0..spent_outputs)
            .map(|_| TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: script_pubkey.clone(),
            })
            .collect::<Vec<_>>();
        parent_outputs.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(vec![0x51; padding_bytes]),
        });
        let parent = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([76; 32]), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: parent_outputs,
        };
        let unsigned = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: (0..spent_outputs)
                .map(|vout| TxIn {
                    previous_output: OutPoint::new(parent.compute_txid(), vout as u32),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                })
                .collect(),
            output: vec![TxOut {
                value: Amount::from_sat(49_000 * spent_outputs as u64),
                script_pubkey,
            }],
        };
        let mut psbt = Psbt::from_unsigned_tx(unsigned).unwrap();
        for (index, input) in psbt.inputs.iter_mut().enumerate() {
            input.witness_utxo = Some(parent.output[index].clone());
            input.non_witness_utxo = Some(parent.clone());
        }
        (parent, psbt)
    }

    #[test]
    fn exact_signer_size_counts_each_repeated_full_parent() {
        let (parent, two_inputs) = repeated_parent_psbt(40_000, 2);
        let mut one_input = two_inputs.clone();
        one_input.unsigned_tx.input.truncate(1);
        one_input.inputs.truncate(1);
        let one_size = signer_request_body(&one_input, "mutinynet").unwrap().len();
        let two_size = signer_request_body(&two_inputs, "mutinynet").unwrap().len();
        assert!(two_size > one_size + serialize(&parent).len());
        assert!(two_size < DEFAULT_MAX_SIGNER_REQUEST_BYTES);
    }

    #[test]
    fn one_huge_parent_is_rejected_by_the_default_request_cap() {
        let (_, psbt) = repeated_parent_psbt(800_000, 1);
        let size = signer_request_body(&psbt, "mutinynet").unwrap().len();
        assert!(size > DEFAULT_MAX_SIGNER_REQUEST_BYTES);
        assert!(size > ABSOLUTE_MAX_SIGNER_REQUEST_BYTES);
    }

    #[test]
    fn partial_signed_record_resumes_without_resigning_and_rejects_tampering() {
        let dir = tempdir().unwrap();
        let (artifact, _, _) = bridge_artifact_fixture();
        let mut record = SignedBatchRecord {
            batch_index: 1,
            txid: artifact.txid.clone(),
            artifact_file: "batch-001.json".to_owned(),
            artifact_sha256: maintenance_artifact_digest(&artifact).unwrap(),
            materialized: false,
            artifact,
        };
        assert!(materialize_signed_record(dir.path(), &mut record).unwrap());
        assert!(record.materialized);
        assert!(!materialize_signed_record(dir.path(), &mut record).unwrap());

        fs::write(dir.path().join("batch-001.json"), b"{}\n").unwrap();
        assert!(materialize_signed_record(dir.path(), &mut record).is_err());
    }

    #[test]
    fn batch_plan_digest_detects_safety_parameter_tampering() {
        let identity = bip84_descriptor_identity(
            Xpub::from_str(
                "tpubDC2Qwo2TFsaNC4ju8nrUJ9mqVT3eSgdmy1yPqhgkjwmke3PRXutNGRYAUo6RCHTcVQaDR3ohNU9we59brGHuEKPvH1ags2nevW5opEE9Z5Q",
            )
            .unwrap(),
            Fingerprint::from_str("c55b303f").unwrap(),
            Network::Signet,
            true,
        )
        .unwrap();
        let plan = BatchSetPlan {
            version: BATCH_SET_VERSION,
            plan_digest: String::new(),
            snapshot_tip_height: 1,
            remote_tip_height: 1,
            known_utxo_sync_performed: true,
            revealed_script_sync_performed: true,
            maintenance_mode: MaintenanceMode::WalletReset,
            source_descriptor_identity: identity.clone(),
            destination_descriptor_identity: identity,
            require_drain_all: true,
            eligible_input_count: 0,
            excluded_eligible_input_count: 0,
            preserved_input_count: 0,
            planned_output_count: 0,
            destination: "tb1qfixture".to_owned(),
            destination_keychain: "internal".to_owned(),
            destination_index: 0,
            xpub: "fixture".to_owned(),
            master_fingerprint: "fixture".to_owned(),
            signer_network: "mutinynet".to_owned(),
            exclusion_count: 0,
            exclusion_sha256: exclusion_digest(&HashSet::new()),
            total_input_sats: 0,
            total_output_sats: 0,
            total_fee_sats: 0,
            requested_fee_rate_sat_vb: 3,
            max_inputs_per_batch: 100,
            max_outputs_per_batch: 100,
            max_total_fee_sats: 200_000,
            max_fee_sats_per_batch: 50_000,
            max_weight_wu_per_batch: 200_000,
            max_signer_request_bytes: DEFAULT_MAX_SIGNER_REQUEST_BYTES,
            unsigned_txids: Vec::new(),
            batches: Vec::new(),
        }
        .seal()
        .unwrap();
        let mut tampered = plan.clone();
        tampered.max_signer_request_bytes -= 1;
        assert_ne!(tampered.computed_digest().unwrap(), plan.plan_digest);
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
        assert!(signed.weight().to_wu() <= conservative_signed_p2wpkh_weight(&psbt.unsigned_tx));

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
