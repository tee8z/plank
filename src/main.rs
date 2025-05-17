use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{backend::CrosstermBackend, Terminal};

mod cli;
mod config;

enum SidePanel {
    Receive,
    Send,
}

struct AppState {
    side_panel: SidePanel,
    // config: config::Config,  // Temporarily removed as it's not currently used
}

fn main() -> Result<()> {
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

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = AppState {
        side_panel: SidePanel::Receive,
        // config: config,  // Temporarily removed as it's not currently used
    };

    loop {
        terminal.draw(|f| {
            let size = f.size();
            let main_block = Block::default()
                .title("Plank - Mutinynet Wallet")
                .borders(Borders::ALL);
            f.render_widget(&main_block, size);
            let inner = main_block.inner(size);

            // Split horizontally: top (status+side), bottom (transactions)
            let main_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(50), // Top half
                    Constraint::Percentage(50), // Bottom half
                ])
                .split(inner);

            // Top half: split vertically: left (status), right (side panel)
            let top_layout = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(40), // Status
                    Constraint::Percentage(60), // Send/Receive
                ])
                .split(main_layout[0]);

            // Status block (top left)
            let status_block = Block::default()
                .title("Wallet Status")
                .borders(Borders::ALL);
            let status_lines = vec![
                Line::from("Wallet: plank-wallet (placeholder)"),
                Line::from("Network: signet (placeholder)"),
                Line::from("Balance: -- BTC"),
                Line::from("Block Height: --"),
                Line::from("Sync Status: Not synced (placeholder)"),
                Line::from("Last Sync: never (placeholder)"),
                Line::from("Online: --"),
                Line::from(Span::styled(
                    "Warning: Offline mode!",
                    ratatui::style::Style::default().fg(ratatui::style::Color::Yellow),
                )),
            ];
            let status_content = Paragraph::new(status_lines).block(status_block);
            f.render_widget(status_content, top_layout[0]);

            // Side panel block (top right)
            match app.side_panel {
                SidePanel::Receive => {
                    let receive_block = Block::default()
                        .title("Receive Bitcoin")
                        .borders(Borders::ALL);
                    let receive_lines = vec![
                        Line::from("[ Receive Funds ]"),
                        Line::from(""),
                        Line::from("Address:"),
                        Line::from("  bc1qxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx (placeholder)"),
                        Line::from(""),
                        Line::from("[QR CODE PLACEHOLDER]"),
                        Line::from(""),
                        Line::from("Share this address to receive bitcoin."),
                        Line::from(""),
                        Line::from("[s] Switch to Send"),
                    ];
                    let receive_content = Paragraph::new(receive_lines).block(receive_block);
                    f.render_widget(receive_content, top_layout[1]);
                }
                SidePanel::Send => {
                    let send_block = Block::default().title("Send Bitcoin").borders(Borders::ALL);
                    let send_lines = vec![
                        Line::from("[ Send Funds ]"),
                        Line::from(""),
                        Line::from("Recipient Address:"),
                        Line::from("  (placeholder for input)"),
                        Line::from("Amount (BTC):"),
                        Line::from("  (placeholder for input)"),
                        Line::from("Fee: (auto/placeholder)"),
                        Line::from(""),
                        Line::from("[Send]   [Cancel]"),
                        Line::from(""),
                        Line::from("[r] Switch to Receive"),
                        Line::from(""),
                        Line::from(Span::styled(
                            "Error: (placeholder for error messages)",
                            ratatui::style::Style::default().fg(ratatui::style::Color::Red),
                        )),
                        Line::from(Span::styled(
                            "Info: (placeholder for info messages)",
                            ratatui::style::Style::default().fg(ratatui::style::Color::Green),
                        )),
                    ];
                    let send_content = Paragraph::new(send_lines).block(send_block);
                    f.render_widget(send_content, top_layout[1]);
                }
            }

            // Transactions block (bottom)
            use ratatui::widgets::{Cell, Row, Table};
            let tx_block = Block::default()
                .title("Recent Transactions")
                .borders(Borders::ALL);

            // Mock data
            struct TxRow<'a> {
                txid: &'a str,
                memo: &'a str,
                amount_sats: &'a str,
                balance_sats: &'a str,
            }
            let tx_rows = vec![
                TxRow {
                    txid: "e3c1b3a7b2f0a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2c9e1",
                    memo: "Salary",
                    amount_sats: "+5000000",
                    balance_sats: "15000000",
                },
                TxRow {
                    txid: "b2d4c9e1f1a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2c9e1b2d4",
                    memo: "Coffee at the very best cafe downtown",
                    amount_sats: "-250000",
                    balance_sats: "10000000",
                },
                TxRow {
                    txid: "f1a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2",
                    memo: "",
                    amount_sats: "+10000000",
                    balance_sats: "10250000",
                },
                TxRow {
                    txid: "a7b2e3c1f1a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2c9e1f1a9d8c3e1d4b2c9e1a7b2e3c1",
                    memo: "Rent",
                    amount_sats: "-5000000",
                    balance_sats: "250000",
                },
            ];

            let header = Row::new(vec![
                Cell::from("TxID"),
                Cell::from("Memo"),
                Cell::from("Amount (sats)"),
                Cell::from("Balance (sats)"),
            ])
            .style(
                ratatui::style::Style::default()
                    .fg(ratatui::style::Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );

            let rows = tx_rows.iter().map(|tx| {
                // Truncate memo to 12 chars with ellipsis if needed
                let memo_disp = if tx.memo.is_empty() {
                    "-".to_string()
                } else if tx.memo.chars().count() > 12 {
                    let mut short = tx.memo.chars().take(11).collect::<String>();
                    short.push('…');
                    short
                } else {
                    tx.memo.to_string()
                };
                // Color code amount
                let amount_val = tx.amount_sats.replace(",", "");
                let _amount_style = if amount_val.starts_with('+') {
                    ratatui::style::Style::default().fg(ratatui::style::Color::Green)
                } else if amount_val.starts_with('-') {
                    ratatui::style::Style::default().fg(ratatui::style::Color::Red)
                } else {
                    ratatui::style::Style::default()
                };
                Row::new(vec![
                    Cell::from(tx.txid),
                    Cell::from(memo_disp),
                    Cell::from(tx.amount_sats),
                    Cell::from(tx.balance_sats),
                ])
            });

            let tx_widths = [
                Constraint::Length(66), // TxID (full 64 chars + a little space)
                Constraint::Length(16), // Memo
                Constraint::Length(16), // Amount (sats)
                Constraint::Length(16), // Balance (sats)
            ];
            let tx_table = Table::new(rows, &tx_widths).header(header).block(tx_block);
            f.render_widget(tx_table, main_layout[1]);
        })?;

        // Handle events
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('r') => app.side_panel = SidePanel::Receive,
                KeyCode::Char('s') => app.side_panel = SidePanel::Send,
                _ => {}
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}
