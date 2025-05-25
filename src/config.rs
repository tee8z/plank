use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use dirs::config_dir;
use serde::{Deserialize, Serialize};

use crate::cli::Cli;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    /// URL of the Esplora server
    pub esplora_url: String,
    /// Whether to run in offline mode
    pub offline: bool,
    /// Directory to store wallet data
    pub data_dir: PathBuf,
    /// Enable debug logging
    pub debug: bool,
    /// Name of the wallet
    pub name: String,
}

impl Default for Config {
    fn default() -> Self {
        let mut data_dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        data_dir.push("plank");

        Self {
            esplora_url: "https://mutinynet.com/api".to_string(),
            offline: false,
            data_dir,
            debug: false,
            name: "default".to_string(),
        }
    }
}

impl Config {
    /// Load configuration from file and merge with CLI arguments
    pub fn load() -> Result<Self> {
        let cli = Cli::parse();
        let mut config = Self::load_from_file(&cli)?;

        // Apply CLI overrides
        if let Some(esplora_url) = cli.esplora_url {
            config.esplora_url = esplora_url;
        }

        if cli.offline {
            config.offline = true;
        }

        if let Some(data_dir) = cli.data_dir {
            config.data_dir = data_dir;
        }

        if let Some(name) = cli.name {
            config.name = name;
        }

        Ok(config)
    }

    /// Load configuration from file if it exists, otherwise return default
    fn load_from_file(cli: &Cli) -> Result<Self> {
        let path = match &cli.config {
            Some(path) => path.clone(),
            None => config_path()?,
        };

        if !path.exists() {
            return Ok(Config::default());
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }
}

/// Get the default path to the config file
pub fn config_path() -> Result<PathBuf> {
    let mut path =
        config_dir().ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?;
    path.push("plank");

    // Create config directory if it doesn't exist
    if !path.exists() {
        fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create config directory: {}", path.display()))?;
    }

    path.push("config.toml");
    Ok(path)
}

/// Get the path to the wallet data directory
pub fn data_dir_path(config: &Config) -> PathBuf {
    let path = config.data_dir.clone();

    // Create data directory if it doesn't exist
    if !path.exists() {
        if let Err(e) = fs::create_dir_all(&path) {
            eprintln!("Warning: Failed to create data directory: {}", e);
        }
    }

    path
}
