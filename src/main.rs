use anyhow::Result;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use plank::config;
use plank::ui::App;
use plank::wallet::AppWallet;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments and load configuration
    let config = config::Config::load()?;

    // Initialize logger
    env_logger::Builder::from_default_env()
        .filter_level(if config.debug {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .init();

    log::info!("Starting Plank Wallet");
    log::debug!("Configuration: {:#?}", config);

    // Create data directory if it doesn't exist
    let data_dir = config::data_dir_path(&config);
    log::debug!("Using data directory: {}", data_dir.display());

    let wallet = AppWallet::init(&data_dir, &config.esplora_url, &config.name).await?;

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Create and run the app
    let mut app = App::new(wallet, config);
    app.run()?;

    // Restore terminal
    disable_raw_mode()?;
    execute!(stdout, LeaveAlternateScreen)?;

    Ok(())
}
