use std::io::ErrorKind;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;
use std::{path::Path, str::FromStr};

use anyhow::{Context, Result};
use bdk_esplora::{esplora_client, EsploraAsyncExt};
use bdk_wallet::bitcoin::bip32::{Xpriv, Xpub};
use bdk_wallet::bitcoin::{self, Address, Amount, FeeRate, Network, SignedAmount, TxIn};
use bdk_wallet::descriptor::IntoWalletDescriptor;
use bdk_wallet::rusqlite::Connection;
use bdk_wallet::template::{Bip84, Bip84Public};
use bdk_wallet::{
    AddressInfo, KeychainKind, LocalOutput, PersistedWallet, SignOptions, Wallet, WalletTx,
};
use rand::Rng;
use tokio::fs;

#[derive(Debug, Clone)]
pub struct SyncStatus {
    pub is_syncing: bool,
    pub last_sync: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct AppWallet {
    wallet: Arc<RwLock<PersistedWallet<Connection>>>,
    conn: Arc<Mutex<Connection>>,
    esplora: esplora_client::AsyncClient,
    block_height: Arc<RwLock<u32>>,
    sync_status: Arc<RwLock<SyncStatus>>,
}

impl AppWallet {
    pub async fn init(data_dir: &Path, esplora_url: &str, name: &str) -> Result<Self> {
        let db_filename = format!("{}.db", name);
        let db_path = data_dir.join(db_filename);
        let key_filename = format!("{}.key", name);
        let key_path = data_dir.join(key_filename);

        let mut conn = tokio::task::spawn_blocking(move || Connection::open(db_path)).await??;

        let (mut wallet, created) = match fs::read_to_string(&key_path).await {
            Ok(contents) if contents.starts_with("tprv") => {
                (load_wallet_with_pvt(&mut conn, &contents)?, false)
            }
            Ok(contents) => (load_wallet_with_pub(&mut conn, &contents)?, false),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                (create_wallet(&mut conn, &key_path).await?, true)
            }
            Err(e) => return Err(e).context("Failed to read key file")?,
        };

        let esplora = esplora_client::Builder::new(&esplora_url).build_async()?;
        if created {
            let update = esplora.full_scan(wallet.start_full_scan(), 10, 3).await?;
            wallet.apply_update(update)?;
        }

        let app_wallet = Self {
            wallet: Arc::new(RwLock::new(wallet)),
            conn: Arc::new(Mutex::new(conn)),
            esplora,
            block_height: Arc::new(RwLock::new(0)),
            sync_status: Arc::new(RwLock::new(SyncStatus {
                is_syncing: false,
                last_sync: None,
            })),
        };

        let app_wallet_clone = app_wallet.clone();
        tokio::spawn(async move {
            app_wallet_clone.update_block_height().await;
        });

        Ok(app_wallet)
    }

    async fn update_block_height(&self) {
        loop {
            let new_block_height = self.esplora.get_height().await.unwrap();
            {
                let mut block_height = self.block_height.write().unwrap();
                *block_height = new_block_height;
            }

            if let Err(e) = self.sync().await {
                log::error!("Failed to sync wallet: {:#?}", e);
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
        }
    }

    fn mark_is_syncing(&self) {
        let mut sync_status = self.sync_status.write().unwrap();
        sync_status.is_syncing = true;
    }

    fn mark_synced(&self) {
        let mut sync_status = self.sync_status.write().unwrap();
        sync_status.is_syncing = false;
        sync_status.last_sync = Some(std::time::SystemTime::now());
    }

    pub async fn sync(&self) -> Result<()> {
        self.mark_is_syncing();
        let request = {
            let wallet = self.wallet.read().unwrap();
            wallet.start_sync_with_revealed_spks()
        };

        let update = self.esplora.sync(request, 5).await?;
        let mut wallet = self.wallet.write().unwrap();
        wallet.apply_update(update)?;

        let mut conn = self.conn.lock().unwrap();
        wallet.persist(&mut conn)?;

        self.mark_synced();
        Ok(())
    }

    pub fn get_balance(&self) -> Amount {
        let wallet = self.wallet.read().unwrap();
        wallet.balance().total()
    }

    pub fn get_pending_balance(&self) -> Amount {
        let wallet = self.wallet.read().unwrap();
        let balance = wallet.balance();
        balance.trusted_pending + balance.untrusted_pending
    }

    pub fn get_block_height(&self) -> u32 {
        let block_height = self.block_height.read().unwrap();
        *block_height
    }

    pub fn get_sync_status(&self) -> SyncStatus {
        let sync_status = self.sync_status.read().unwrap();
        sync_status.clone()
    }

    pub fn get_transactions(&self) -> Vec<Transaction> {
        let wallet = self.wallet.read().unwrap();
        wallet
            .transactions_sort_by(|tx1, tx2| tx2.chain_position.cmp(&tx1.chain_position))
            .iter()
            .map(|tx| Transaction::from_wallet_transaction(&tx, &*wallet))
            .collect()
    }

    pub fn get_utxos(&self) -> Vec<LocalOutput> {
        let wallet = self.wallet.read().unwrap();
        wallet.list_unspent().collect()
    }

    pub fn new_address(&self) -> Result<AddressInfo> {
        let mut wallet = self.wallet.write().unwrap();
        let address = wallet.reveal_next_address(KeychainKind::External);

        let mut conn = self.conn.lock().unwrap();
        wallet.persist(&mut conn)?;

        Ok(address)
    }

    pub async fn send(&self, address: &Address, amount: u64) -> Result<bitcoin::Txid> {
        let tx = {
            let mut wallet = self.wallet.write().unwrap();

            let mut builder = wallet.build_tx();
            builder
                .add_recipient(address.script_pubkey(), Amount::from_sat(amount))
                .fee_rate(FeeRate::from_sat_per_vb(2).unwrap());
            let mut psbt = builder.finish()?;

            if !wallet.sign(&mut psbt, SignOptions::default())? {
                return Err(anyhow::anyhow!("Failed to sign transaction"));
            }

            psbt.extract_tx()?
        };

        self.esplora.broadcast(&tx).await?;

        {
            let mut wallet = self.wallet.write().unwrap();
            let mut conn = self.conn.lock().unwrap();
            wallet.persist(&mut conn)?;
        }

        Ok(tx.compute_txid())
    }
}

#[derive(Debug, Clone)]
pub struct Txid(bitcoin::Txid);

impl std::ops::Deref for Txid {
    type Target = bitcoin::Txid;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for Txid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0.to_string();
        if s.len() <= 30 {
            // If the string is short, just display it as is
            write!(f, "{}", s)
        } else {
            // Otherwise, show first 15 and last 15 characters with ... in between
            write!(f, "{}...{}", &s[..15], &s[s.len() - 15..])
        }
    }
}

#[derive(Debug, Clone)]
pub struct Transaction {
    pub id: Txid,
    pub incoming_amount: Amount,
    pub outgoing_amount: Amount,
}

impl Transaction {
    pub fn from_wallet_transaction(tx: &WalletTx, wallet: &PersistedWallet<Connection>) -> Self {
        let tx = &tx.tx_node.tx;

        let incoming_amount = tx
            .output
            .iter()
            .filter(|output| wallet.is_mine(output.script_pubkey.clone()))
            .map(|output| output.value)
            .sum();
        let outgoing_amount = tx
            .input
            .iter()
            .filter_map(|input| get_input_value(wallet, input))
            .sum();

        Self {
            id: Txid(tx.compute_txid()),
            incoming_amount,
            outgoing_amount,
        }
    }

    pub fn net_amount(&self) -> SignedAmount {
        let incoming_amount = self.incoming_amount.to_signed().unwrap();
        let outgoing_amount = self.outgoing_amount.to_signed().unwrap();

        if incoming_amount > outgoing_amount {
            incoming_amount - outgoing_amount
        } else {
            outgoing_amount - incoming_amount
        }
    }
}

fn get_input_value(wallet: &PersistedWallet<Connection>, input: &TxIn) -> Option<Amount> {
    wallet
        .list_output()
        .find(|output| output.outpoint == input.previous_output)
        .map(|output| output.txout.value)
}

async fn create_wallet(
    conn: &mut Connection,
    key_path: &Path,
) -> Result<PersistedWallet<Connection>> {
    let mut data: [u8; 64] = [0; 64];
    rand::rng().fill(&mut data);

    let xpriv = Xpriv::new_master(Network::Signet, &data)?;
    fs::write(key_path, xpriv.to_string()).await?;

    init_new_wallet(
        conn,
        Bip84(xpriv, KeychainKind::External),
        Bip84(xpriv, KeychainKind::Internal),
    )
}

fn load_wallet_with_pvt(
    conn: &mut Connection,
    key_contents: &str,
) -> Result<PersistedWallet<Connection>> {
    let key = Xpriv::from_str(key_contents)?;
    let d1 = Bip84(key, KeychainKind::External);
    let d2 = Bip84(key, KeychainKind::Internal);

    if let Some(wallet) = load_wallet(conn, d1.clone(), d2.clone(), true)? {
        Ok(wallet)
    } else {
        // no changeset. file was created and then tui booted up. consider it an import of a key.
        // so we need to create a new wallet
        init_new_wallet(conn, d1, d2)
    }
}

fn load_wallet_with_pub(
    conn: &mut Connection,
    key_contents: &str,
) -> Result<PersistedWallet<Connection>> {
    let key = Xpub::from_str(key_contents)?;
    let fingerprint = key.fingerprint();
    let d1 = Bip84Public(key, fingerprint, KeychainKind::External);
    let d2 = Bip84Public(key, fingerprint, KeychainKind::Internal);

    if let Some(wallet) = load_wallet(conn, d1.clone(), d2.clone(), false)? {
        Ok(wallet)
    } else {
        // no changeset. file was created and then tui booted up. consider it an import of a key.
        // so we need to create a new wallet
        init_new_wallet(conn, d1, d2)
    }
}

fn init_new_wallet<D>(
    conn: &mut Connection,
    descriptor_1: D,
    descriptor_2: D,
) -> Result<PersistedWallet<Connection>>
where
    D: IntoWalletDescriptor + Send + Clone + 'static,
{
    let wallet = Wallet::create(descriptor_1, descriptor_2)
        .network(Network::Signet)
        .create_wallet(conn)?;

    Ok(wallet)
}

fn load_wallet<D, E>(
    conn: &mut Connection,
    descriptor_1: D,
    descriptor_2: E,
    is_pvt: bool,
) -> Result<Option<PersistedWallet<Connection>>>
where
    D: IntoWalletDescriptor + Send + 'static,
    E: IntoWalletDescriptor + Send + 'static,
{
    let mut wallet = bdk_wallet::Wallet::load()
        .descriptor(KeychainKind::External, Some(descriptor_1))
        .descriptor(KeychainKind::Internal, Some(descriptor_2))
        .check_network(Network::Signet);

    if is_pvt {
        wallet = wallet.extract_keys();
    }

    let wallet = wallet.load_wallet(conn)?;

    Ok(wallet)
}
