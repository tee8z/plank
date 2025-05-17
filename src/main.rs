use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{backend::CrosstermBackend, Terminal};

mod config;

fn main() -> Result<()> {
    // Load configuration
    let config = config::Config::load();

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Draw the screen with config info or error
    terminal.draw(|f| {
        let size = f.size();
        let block = Block::default()
            .title("Plank - Mutinynet Wallet")
            .borders(Borders::ALL);
        f.render_widget(&block, size);

        let inner = block.inner(size);
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(inner);

        let content = match &config {
            Ok(cfg) => vec![
                Line::from(format!("Esplora URL: {}", cfg.esplora_url)),
                Line::from(format!("Offline: {}", cfg.offline)),
            ],
            Err(e) => vec![Line::from(Span::styled(
                format!("Failed to load config: {}", e),
                ratatui::style::Style::default().fg(ratatui::style::Color::Red),
            ))],
        };
        let paragraph = Paragraph::new(content);
        f.render_widget(paragraph, layout[0]);
    })?;

    // Wait for a key press
    loop {
        if let Event::Key(key) = event::read()? {
            if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                break;
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}
