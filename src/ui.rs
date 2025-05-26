use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::Result;
use arboard::Clipboard;
use bdk_wallet::bitcoin::{Address, Amount, Denomination, Network, SignedAmount};
use crossterm::event::KeyEvent;
use crossterm::event::{self, Event, KeyCode};
use ratatui::text::Text;
use ratatui::widgets::Padding;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table},
    Frame, Terminal,
};
use tui_textarea::TextArea;

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

#[derive(Debug, Clone, thiserror::Error)]
enum ModalErr {
    #[error("No address provided")]
    EmptyAddress,
    #[error("No amount provided")]
    EmptyAmount,
    #[error("Invalid address: {0}")]
    InvalidAddress(String),
    #[error("Invalid amount: {0}")]
    InvalidAmount(String),
}

pub struct SendModal {
    address_input: TextArea<'static>,
    amount_input: TextArea<'static>,
    active_input: usize,
    show_modal: bool,
}

impl Default for SendModal {
    fn default() -> Self {
        let mut address_input = TextArea::default();
        let mut amount_input = TextArea::default();

        // Configure address input
        address_input.set_placeholder_text("Enter recipient address");
        address_input.set_cursor_line_style(Style::default());

        // Configure amount input
        amount_input.set_placeholder_text("Enter amount in BTC");
        amount_input.set_cursor_line_style(Style::default());

        Self {
            address_input,
            amount_input,
            active_input: 0,
            show_modal: false,
        }
    }
}

impl SendModal {
    fn toggle(&mut self) {
        self.show_modal = !self.show_modal;
        if self.show_modal {
            self.active_input = 0;
            let mut address_input = TextArea::default();
            let mut amount_input = TextArea::default();

            // Configure address input
            address_input.set_placeholder_text("Enter recipient address");
            address_input.set_cursor_line_style(Style::default());

            // Configure amount input
            amount_input.set_placeholder_text("Enter amount in BTC");
            amount_input.set_cursor_line_style(Style::default());

            self.address_input = address_input;
            self.amount_input = amount_input;
        }
    }

    fn handle_input(&mut self, input: KeyEvent) -> bool {
        match input.code {
            KeyCode::Tab => {
                self.active_input = (self.active_input + 1) % 2;
                set_input_styles(&mut self.address_input, self.active_input == 0);
                set_input_styles(&mut self.amount_input, self.active_input == 1);
                true
            }
            KeyCode::Esc => {
                self.show_modal = false;
                false
            }
            _ => {
                let current_input = match self.active_input {
                    0 => &mut self.address_input,
                    1 => &mut self.amount_input,
                    _ => return false,
                };
                current_input.input(input);
                true
            }
        }
    }

    /// Get the form data if it's valid
    fn get_form_data(&self) -> Result<(Address, u64), ModalErr> {
        let address = self.address_input.lines().join("").trim().to_string();
        if address.is_empty() {
            return Err(ModalErr::EmptyAddress);
        }

        let address = Address::from_str(&address)
            .and_then(|addr| addr.require_network(Network::Signet))
            .map_err(|_| ModalErr::InvalidAddress(address))?;

        let amount_str = self.amount_input.lines().join("").trim().to_string();
        if amount_str.is_empty() {
            return Err(ModalErr::EmptyAmount);
        }

        let amount = amount_str
            .parse::<u64>()
            .map_err(|_| ModalErr::InvalidAmount(amount_str))?;

        Ok((address, amount))
    }

    /// Render the send bitcoin modal dialog
    fn render_modal(&mut self, f: &mut Frame, area: Rect) {
        // Clear the area
        let block = Block::default()
            .title(" Send Bitcoin ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::LightBlue));

        f.render_widget(Clear, area);
        f.render_widget(block, area);

        // Inner area with padding
        let inner = area.inner(&Margin {
            horizontal: 2,
            vertical: 1,
        });

        // Layout for the form
        let chunks = Layout::vertical([
            Constraint::Length(1),  // Address label
            Constraint::Length(10), // Address input
            Constraint::Length(1),  // Spacer
            Constraint::Length(1),  // Amount label
            Constraint::Length(10), // Amount input
            Constraint::Min(1),     // Spacer
            Constraint::Length(3),  // Buttons
        ])
        .split(inner);

        // Render address label
        let address_label = Paragraph::new("Recipient Address:")
            .style(Style::default().add_modifier(Modifier::BOLD));
        f.render_widget(address_label, chunks[0]);

        // Configure and render address input
        let address_input = self.address_input.clone();
        f.render_widget(address_input.widget(), chunks[1]);

        // Render amount label
        let amount_label =
            Paragraph::new("Amount (sats):").style(Style::default().add_modifier(Modifier::BOLD));
        f.render_widget(amount_label, chunks[3]);

        // Configure and render amount input
        let amount_input = self.amount_input.clone();
        f.render_widget(amount_input.widget(), chunks[4]);

        // Render buttons
        let button_area =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(chunks[6]);

        // Cancel button
        let cancel_button = Paragraph::new("Cancel (Esc)")
            .centered()
            .style(Style::default().fg(Color::Gray));

        // Send button
        let send_button = Paragraph::new("Send (Enter)")
            .centered()
            .style(Style::default().fg(Color::Gray));

        f.render_widget(cancel_button, button_area[0]);
        f.render_widget(send_button, button_area[1]);

        // Set cursor position for active input
        if self.active_input == 0 {
            let (_, col) = self.address_input.cursor();
            f.set_cursor(chunks[1].x + col as u16, chunks[1].y);
        } else {
            let (_, col) = self.amount_input.cursor();
            f.set_cursor(chunks[4].x + col as u16, chunks[4].y);
        }
    }

    fn render(&mut self, f: &mut Frame) {
        if !self.show_modal {
            return;
        }

        // Create a centered area for the modal
        let area = centered_rect(60, 20, f.size());
        self.render_modal(f, area);
    }
}

fn set_input_styles(input: &mut TextArea<'_>, active: bool) {
    if active {
        input.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    } else {
        input.set_cursor_style(Style::default());
    }
}

/// Helper function to center a rectangle with given width and height
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

pub struct App {
    pub should_quit: bool,
    wallet: AppWallet,
    config: Config,
    toast: Option<Toast>,
    send_modal: SendModal,
}

impl App {
    pub fn new(wallet: AppWallet, config: Config) -> Self {
        Self {
            should_quit: false,
            wallet,
            config,
            toast: None,
            send_modal: SendModal::default(),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        let stdout = std::io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Main event loop
        while !self.should_quit {
            terminal.draw(|f| self.draw(f))?;
            self.handle_events().await?;
        }

        Ok(())
    }

    fn draw(&mut self, f: &mut Frame) {
        let size = f.size();

        // Split the main area into content and help bar
        let [content_area, help_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(size);

        // Main container with border around the content area
        let main_block = Block::bordered().title("Plank - Mutinynet Wallet");

        // Help bar at the bottom
        let help_text = Line::from(vec![
            Span::styled("q:", Style::default().bold()),
            Span::raw(" Quit  "),
            Span::styled("r:", Style::default().bold()),
            Span::raw(" Receive  "),
            Span::styled("s:", Style::default().bold()),
            Span::raw(" Send  "),
            Span::styled("Esc:", Style::default().bold()),
            Span::raw(" Cancel"),
        ]);

        let help_bar = Paragraph::new(help_text)
            .style(Style::default().fg(Color::Gray))
            .alignment(Alignment::Center);

        // Draw toast notification if it exists and not expired
        if let Some(toast) = &self.toast {
            if !toast.is_expired() {
                let notification = Paragraph::new(Line::from(vec![Span::from(toast)])).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .style(Style::default().bg(ratatui::style::Color::DarkGray)),
                );

                // Position the toast above the help bar
                let area = Rect {
                    x: size.width / 4,
                    y: help_area.y - 4, // Position above help area
                    width: size.width / 2,
                    height: 3,
                };
                f.render_widget(notification, area);
            } else {
                // Clear expired toast
                self.toast = None;
            }
        }

        // Draw the send modal if it's visible
        self.send_modal.render(f);
        if self.send_modal.show_modal {
            return;
        }

        // Split the inner area into left (wallet info) and right (transactions) sections with a line in between
        let inner_area = main_block.inner(content_area);
        let [left, _center, right] = Layout::horizontal([
            Constraint::Percentage(49),
            Constraint::Length(1), // For the vertical line
            Constraint::Percentage(50),
        ])
        .areas(inner_area);

        // Draw the main content and help bar
        f.render_widget(main_block, content_area);
        f.render_widget(help_bar, help_area);

        // Draw the left side (wallet info)
        self.draw_wallet_info(f, left);
        // Draw the right side
        let [top, bottom] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);

        self.draw_transactions(f, top);
        self.draw_utxos(f, bottom);
    }

    fn draw_wallet_info(&self, f: &mut Frame, area: Rect) {
        // Create a centered layout for the wallet info
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(3), // 3 lines of text
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
        let balance_text = format!("Balance: {}", format_amount(&self.wallet.get_balance()));
        let balance_text = Line::from(Span::styled(balance_text, Style::default().green()));

        // Pending
        let pending_text = format!(
            "Pending: {}",
            format_amount(&self.wallet.get_pending_balance())
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
                    Cell::from(Text::from(format_signed_amount(&tx.net_amount())).right_aligned()),
                ])
            })
            .collect();

        // Create the table
        let table = Table::new(rows, &[Constraint::Length(40), Constraint::Min(1)])
            .header(
                Row::new(vec!["ID".into(), Text::from("Net").right_aligned()])
                    .style(Style::default().bold())
                    .bottom_margin(1),
            )
            .block(
                Block::bordered()
                    .title("Transactions")
                    .padding(Padding::horizontal(1)),
            )
            .column_spacing(1);

        f.render_widget(table, tx_table);
    }

    fn draw_utxos(&self, f: &mut Frame, area: Rect) {
        let [utxo_table] = Layout::vertical([Constraint::Min(1)]).areas(area);

        let rows: Vec<Row> = self
            .wallet
            .get_utxos()
            .iter()
            .map(|tx| {
                Row::new(vec![
                    Cell::from(tx.outpoint.to_string()),
                    Cell::from(Text::from(format_amount(&tx.txout.value)).right_aligned()),
                ])
            })
            .collect();

        // Create the table
        let table = Table::new(rows, &[Constraint::Length(64), Constraint::Min(25)])
            .header(
                Row::new(vec!["ID".into(), Text::from("Value").right_aligned()])
                    .style(Style::default().bold())
                    .bottom_margin(1),
            )
            .block(
                Block::bordered()
                    .title("UTXOs")
                    .padding(Padding::horizontal(1)),
            )
            .column_spacing(1);

        f.render_widget(table, utxo_table);
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
                    self.show_toast(Toast::error(format!(
                        "Failed to copy address to clipboard: {}",
                        e
                    )));
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

    /// Handle submission of the send form
    async fn handle_send_submit(&mut self) -> bool {
        match self.send_modal.get_form_data() {
            Ok((address, amount)) => match self.wallet.send(&address, amount).await {
                Ok(txid) => {
                    self.show_toast(Toast::success(format!("Transaction {} broadcasted", txid)));
                    self.send_modal.show_modal = false;
                    match Clipboard::new() {
                        Ok(mut clipboard) => {
                            if let Err(e) = clipboard.set_text(txid.to_string()) {
                                self.show_toast(Toast::error(format!(
                                    "Failed to copy transaction ID to clipboard: {}",
                                    e
                                )));
                            } else {
                                self.show_toast(Toast::success(format!(
                                    "Transaction ID copied to clipboard: {}",
                                    txid
                                )));
                            }
                        }
                        Err(e) => {
                            self.show_toast(Toast::error(format!(
                                "Failed to access clipboard: {}",
                                e
                            )));
                        }
                    }
                    true
                }
                Err(e) => {
                    self.show_toast(Toast::error(format!("Failed to send: {}", e)));
                    false
                }
            },
            Err(e) => {
                self.show_toast(Toast::error(e.to_string()));
                false
            }
        }
    }

    /// Handle key press events
    async fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        if self.send_modal.show_modal {
            return match key.code {
                KeyCode::Enter => self.handle_send_submit().await,
                _ => self.send_modal.handle_input(key),
            };
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('r') => self.handle_new_address(),
            KeyCode::Char('s') => {
                self.send_modal.toggle();
                return false; // Event handled, stop further processing
            }
            _ => {}
        }
        true
    }
    /// Handle all input events
    async fn handle_events(&mut self) -> Result<()> {
        if !event::poll(std::time::Duration::from_millis(100))? {
            return Ok(());
        }

        if let Event::Key(key) = event::read()? {
            let event_handled = self.handle_key_event(key).await;
            if event_handled {
                return Ok(());
            }
        }

        Ok(())
    }
}

fn format_amount(amount: &Amount) -> String {
    format!("{} sats", amount.display_in(Denomination::SAT))
}

fn format_signed_amount(amount: &SignedAmount) -> String {
    format!("{} sats", amount.display_in(Denomination::SAT))
}
