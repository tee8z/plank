use anyhow::Result;
use arboard::Clipboard;
use crossterm::event::KeyEvent;
use crossterm::event::{self, Event, KeyCode};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::components::*;
use crate::config::Config;
use crate::wallet::AppWallet;

pub struct App {
    pub should_quit: bool,
    wallet: AppWallet,
    toast: Option<Toast>,
    send_modal: SendModal,
    transactions_table: TransactionsTable,
    wallet_info: WalletInfo,
    utxos_table: UtxosTable,
}

impl App {
    pub fn new(wallet: AppWallet, config: Config) -> Self {
        Self {
            should_quit: false,
            wallet: wallet.clone(),
            toast: None,
            send_modal: SendModal::default(),
            transactions_table: TransactionsTable::new(wallet.clone()),
            wallet_info: WalletInfo::new(&config.name, wallet.clone()),
            utxos_table: UtxosTable::new(wallet),
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
        if self.send_modal.visible() {
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
        self.wallet_info.render(f, left);
        // Draw the right side
        let [top, bottom] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);

        self.transactions_table.render(f, top);
        self.utxos_table.render(f, bottom);
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
                    self.send_modal.toggle();
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
        if self.send_modal.visible() {
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
