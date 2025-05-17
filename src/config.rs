use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use dirs::config_dir;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub esplora_url: String,
    pub offline: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            esplora_url: "https://mutinynet.com/esplora".to_string(),
            offline: false,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Config::default());
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
        Ok(config)
    }
}

pub fn config_path() -> PathBuf {
    let mut path = config_dir().expect("Could not determine config directory");
    path.push("plank");
    path.push("config.toml");
    path
}
