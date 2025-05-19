use clap::Parser;
use std::path::PathBuf;

/// A lightweight, secure Bitcoin wallet for the Mutinynet signet network
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Optional name of the wallet to use
    pub name: Option<String>,

    /// Path to configuration file
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Run in offline mode (don't connect to any network)
    #[arg(long)]
    pub offline: bool,

    /// Esplora server URL (e.g., https://mutinynet.com/api)
    #[arg(long, value_name = "URL")]
    pub esplora_url: Option<String>,

    /// Wallet data directory
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    pub debug: bool,
}

impl Cli {
    pub fn parse() -> Self {
        Self::parse_from(std::env::args())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_args() {
        let cli = Cli::parse_from(
            [
                "plank",
                "--esplora-url",
                "https://example.com",
                "--offline",
                "--debug",
            ]
            .iter(),
        );

        assert_eq!(cli.esplora_url, Some("https://example.com".to_string()));
        assert!(cli.offline);
        assert!(cli.debug);
    }
}
