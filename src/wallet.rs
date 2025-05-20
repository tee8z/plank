use std::io::ErrorKind;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;
use std::{path::Path, str::FromStr};

use anyhow::{Context, Result};
use bdk_esplora::{esplora_client, EsploraAsyncExt};
use bdk_wallet::bitcoin::bip32::Xpriv;
use bdk_wallet::bitcoin::{Amount, Network, TxIn, Txid};
use bdk_wallet::rusqlite::Connection;
use bdk_wallet::template::Bip84;
use bdk_wallet::{AddressInfo, KeychainKind, PersistedWallet, Wallet, WalletTx};
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
    pub async fn init(data_dir: &Path, esplora_url: String, name: &str) -> Result<Self> {
        let db_filename = format!("{}.db", name);
        let db_path = data_dir.join(db_filename);
        let key_filename = format!("{}.pvt", name);
        let key_path = data_dir.join(key_filename);

        let mut conn = tokio::task::spawn_blocking(move || Connection::open(db_path)).await??;

        let (mut wallet, created) = match fs::read_to_string(&key_path).await {
            Ok(contents) => (load_wallet(&mut conn, &contents)?, false),
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
            .transactions()
            .map(|tx| Transaction::from_wallet_transaction(&tx, &*wallet))
            .collect()
    }

#[derive(Debug, Clone)]
pub struct Transaction {
    pub id: Txid,
    pub memo: String,
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
            .filter(|input| wallet.is_mine(input.script_sig.clone()))
            .map(|input| get_input_value(wallet, input))
            .sum();

        Self {
            id: tx.compute_txid(),
            memo: "".to_string(),
            incoming_amount,
            outgoing_amount,
        }
    }
}

fn get_input_value(wallet: &PersistedWallet<Connection>, input: &TxIn) -> Amount {
    wallet.get_utxo(input.previous_output).unwrap().txout.value
}

async fn create_wallet(
    conn: &mut Connection,
    key_path: &Path,
) -> Result<PersistedWallet<Connection>> {
    let mut data: [u8; 64] = [0; 64];
    rand::rng().fill(&mut data);

    let xpriv = Xpriv::new_master(Network::Signet, &data)?;
    fs::write(key_path, xpriv.to_string()).await?;

    let wallet = Wallet::create(
        Bip84(xpriv, KeychainKind::External),
        Bip84(xpriv, KeychainKind::Internal),
    )
    .network(Network::Signet)
    .create_wallet(conn)?;

    Ok(wallet)
}

fn load_wallet(conn: &mut Connection, key_contents: &str) -> Result<PersistedWallet<Connection>> {
    let key = Xpriv::from_str(key_contents)?;

    let wallet = bdk_wallet::Wallet::load()
        .descriptor(
            KeychainKind::External,
            Some(Bip84(key, KeychainKind::External)),
        )
        .descriptor(
            KeychainKind::Internal,
            Some(Bip84(key, KeychainKind::Internal)),
        )
        .extract_keys()
        .check_network(Network::Signet)
        .load_wallet(conn)?
        .unwrap();

    Ok(wallet)
}
