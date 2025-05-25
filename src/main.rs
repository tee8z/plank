use anyhow::Result;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

mod ui;

#[tokio::main]
async fn main() -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Create and run the app
    let mut app = ui::App::new();
    app.run()?;

    // Restore terminal
    disable_raw_mode()?;
    execute!(stdout, LeaveAlternateScreen)?;

    Ok(())
}
