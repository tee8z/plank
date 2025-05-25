use std::time::{Duration, Instant};

use anyhow::Result;
use arboard::Clipboard;
use crossterm::event::{self, Event, KeyCode};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame, Terminal,
};

use crate::config::Config;
use crate::wallet::AppWallet;

#[derive(Debug, Clone)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl Default for ToastLevel {
    fn default() -> Self {
        Self::Info
    }
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub level: ToastLevel,
    pub timestamp: Instant,
    pub duration: Duration,
}

impl Toast {
    pub fn new(message: impl Into<String>, level: ToastLevel) -> Self {
        Self {
            message: message.into(),
            level,
            timestamp: Instant::now(),
            duration: Duration::from_secs(3),
        }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self::new(message, ToastLevel::Info)
    }

    pub fn success(message: impl Into<String>) -> Self {
        Self::new(message, ToastLevel::Success)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(message, ToastLevel::Warning)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(message, ToastLevel::Error)
    }

    pub fn is_expired(&self) -> bool {
        self.timestamp.elapsed() > self.duration
    }
}

impl<'a> From<&'a Toast> for Span<'a> {
    fn from(toast: &'a Toast) -> Self {
        let (icon, style) = match toast.level {
            ToastLevel::Info => ("i", Style::default().fg(ratatui::style::Color::LightBlue)),
            ToastLevel::Success => ("✓", Style::default().fg(ratatui::style::Color::Green)),
            ToastLevel::Warning => ("!", Style::default().fg(ratatui::style::Color::Yellow)),
            ToastLevel::Error => ("✗", Style::default().fg(ratatui::style::Color::Red)),
        };

        Span::styled(format!("{} {}", icon, toast.message), style)
    }
}

pub struct App {
    pub should_quit: bool,
    wallet: AppWallet,
    config: Config,
    toast: Option<Toast>,
}

impl App {
    pub fn new(wallet: AppWallet, config: Config) -> Self {
        Self {
            should_quit: false,
            wallet,
            config,
            toast: None,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let stdout = std::io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Main event loop
        while !self.should_quit {
            terminal.draw(|f| self.draw(f))?;
            self.handle_events()?;
        }

        Ok(())
    }

    fn draw(&mut self, f: &mut Frame) {
        let size = f.size();

        // Main container with border around the entire application
        let main_block = Block::default()
            .borders(Borders::ALL)
            .title("Plank - Mutinynet Wallet");

        // Split the inner area into left (wallet info) and right (transactions) sections with a line in between
        let inner_area = main_block.inner(size);
        let [left, _center, right] = Layout::horizontal([
            Constraint::Percentage(49),
            Constraint::Length(1), // For the vertical line
            Constraint::Percentage(50),
        ])
        .areas(inner_area);

        // Draw the main block
        f.render_widget(main_block, size);

        // Draw toast notification if it exists and not expired
        if let Some(toast) = &self.toast {
            if !toast.is_expired() {
                let notification = Paragraph::new(Line::from(vec![Span::from(toast)])).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .style(Style::default().bg(ratatui::style::Color::DarkGray)),
                );

                let area = Rect {
                    x: size.width / 4,
                    y: size.height - 3,
                    width: size.width / 2,
                    height: 3,
                };
                f.render_widget(notification, area);
            } else {
                // Clear expired toast
                self.toast = None;
            }
        }

        // Draw the left side (wallet info)
        self.draw_wallet_info(f, left);
        // Draw the right side
        self.draw_transactions(f, right);
    }

    fn draw_wallet_info(&self, f: &mut Frame, area: Rect) {
        // Create a centered layout for the wallet info
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(3), // 4 lines of text
                Constraint::Min(1),
            ])
            .split(area);

        let container = Layout::horizontal([
            Constraint::Min(1),
            Constraint::Max(40), // Maximum width for the content
            Constraint::Min(1),
        ])
        .split(chunks[1])[1]; // Take the middle chunk

        // Wallet name
        let name_text = Line::from(vec![
            Span::from("Name: "),
            Span::styled(&self.config.name, Style::default().bold()),
        ]);

        // Balance
        let balance_text = format!("Balance: {}", self.wallet.get_balance().display_dynamic());
        let balance_text = Line::from(Span::styled(balance_text, Style::default().green()));

        // Pending
        let pending_text = format!(
            "Pending: {}",
            self.wallet.get_pending_balance().display_dynamic()
        );
        let pending_text = Line::from(Span::styled(pending_text, Style::default().yellow()));

        // Create a single paragraph with all lines
        let text = Text::from(vec![name_text, balance_text, pending_text]);
        let info = Paragraph::new(text).centered();
        f.render_widget(info, container);
    }

    fn draw_transactions(&self, f: &mut Frame, area: Rect) {
        // Draw the transactions section
        let [tx_table] = Layout::vertical([
            Constraint::Min(1), // Transactions list
        ])
        .areas(area);

        // Create table rows
        let rows: Vec<Row> = self
            .wallet
            .get_transactions()
            .iter()
            .map(|tx| {
                Row::new(vec![
                    Cell::from(tx.id.to_string()),
                    Cell::from(tx.memo.clone()),
                    Cell::from(tx.incoming_amount.display_dynamic().to_string()),
                    Cell::from(tx.outgoing_amount.display_dynamic().to_string()),
                ])
            })
            .collect();

        // Create the table
        let table = Table::new(
            rows,
            &[
                Constraint::Length(25),
                Constraint::Min(15),
                Constraint::Percentage(30),
                Constraint::Percentage(30),
            ],
        )
        .header(
            Row::new(vec!["ID", "Memo", "Incoming", "Outgoing"])
                .style(Style::default().add_modifier(Modifier::BOLD))
                .bottom_margin(1),
        )
        .block(Block::default())
        .column_spacing(1);

        f.render_widget(table, tx_table);
    }

    /// Show a toast notification
    pub fn show_toast(&mut self, toast: Toast) {
        self.toast = Some(toast);
    }

    /// Handle clipboard operations for new addresses
    fn handle_new_address(&mut self) {
        let address_info = match self.wallet.new_address() {
            Ok(info) => info,
            Err(e) => {
                self.show_toast(Toast::error(format!("Failed to generate address: {}", e)));
                return;
            }
        };

        let address = address_info.address.to_string();
        match Clipboard::new() {
            Ok(mut clipboard) => {
                if let Err(e) = clipboard.set_text(address.clone()) {
                    self.show_toast(Toast::error(format!("Failed to copy to clipboard: {}", e)));
                } else {
                    self.show_toast(Toast::success(format!(
                        "Address copied to clipboard: {}",
                        address
                    )));
                }
            }
            Err(e) => {
                self.show_toast(Toast::error(format!("Failed to access clipboard: {}", e)));
            }
        }
    }

    /// Handle key press events
    fn handle_key_event(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('r') => self.handle_new_address(),
            _ => {}
        }
    }

    /// Handle all input events
    fn handle_events(&mut self) -> Result<()> {
        if !event::poll(std::time::Duration::from_millis(100))? {
            return Ok(());
        }

        if let Event::Key(key) = event::read()? {
            self.handle_key_event(key.code);
        }

        Ok(())
    }
}
