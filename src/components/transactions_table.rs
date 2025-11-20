use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::Text,
    widgets::{Block, Cell, Padding, Row, Table, TableState},
    Frame,
};

use crate::components::Toast;
use crate::utils::{format_signed_amount, short_tx_id};
use crate::wallet::AppWallet;

#[derive(Debug, Clone)]
pub struct TransactionsTable {
    wallet: AppWallet,
    state: TableState,
    toast_callback: Option<fn(&str) -> Toast>,
}

impl TransactionsTable {
    pub fn new(wallet: AppWallet) -> Self {
        Self {
            wallet,
            state: TableState::default().with_selected(0),
            toast_callback: None,
        }
    }

    pub fn set_toast_callback(&mut self, callback: fn(&str) -> Toast) {
        self.toast_callback = Some(callback);
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, active: bool) {
        // Draw the transactions section
        let [tx_table] = Layout::vertical([
            Constraint::Min(1), // Transactions list
        ])
        .areas(area);

        // Create table rows with highlighting for recent transactions
        let rows: Vec<Row> = self
            .wallet
            .get_transactions()
            .iter()
            .map(|tx| {
                let mut row = Row::new(vec![
                    Cell::from(short_tx_id(&tx.id)),
                    Cell::from(Text::from(format_signed_amount(&tx.net_amount())).right_aligned()),
                ]);

                // Highlight recently created transactions
                if self.wallet.is_transaction_recent(&tx.id) {
                    row = row.style(Style::default().bg(ratatui::style::Color::Green));
                }

                row
            })
            .collect();

        let mut block = Block::bordered()
            .title("Transactions")
            .padding(Padding::horizontal(1));

        if active {
            block = block.border_style(Style::default().red());
        }

        // Create the table
        let mut table = Table::new(rows, &[Constraint::Length(40), Constraint::Min(1)])
            .header(
                Row::new(vec!["ID".into(), Text::from("Net").right_aligned()])
                    .style(Style::default().bold())
                    .bottom_margin(1),
            )
            .block(block)
            .column_spacing(1);

        let mut style = Style::default();
        if active {
            style = style.reversed();
        }

        table = table.highlight_style(style);

        f.render_stateful_widget(table, tx_table, &mut self.state);
    }

    pub fn handle_input(&mut self, input: KeyEvent) -> (bool, Option<Toast>) {
        let count = self.wallet.get_transactions().len();
        let mut toast = None;

        match input.code {
            KeyCode::Down => {
                self.state
                    .select(Some((self.state.selected().unwrap() + 1) % count));
                (true, None)
            }
            KeyCode::Up => {
                self.state
                    .select(Some((self.state.selected().unwrap() + count - 1) % count));
                (true, None)
            }
            KeyCode::Char('c') => {
                // Copy selected transaction ID to clipboard
                if let Some(selected) = self.state.selected() {
                    let transactions = self.wallet.get_transactions();
                    if let Some(tx) = transactions.get(selected) {
                        match Clipboard::new() {
                            Ok(mut clipboard) => {
                                if let Err(e) = clipboard.set_text(tx.id.to_string()) {
                                    toast = Some(Toast::error(format!(
                                        "Failed to copy transaction ID to clipboard: {}",
                                        e
                                    )));
                                } else {
                                    toast = Some(Toast::success_long(format!(
                                        "Transaction ID copied to clipboard: {}",
                                        tx.id
                                    )));
                                }
                            }
                            Err(e) => {
                                toast = Some(Toast::error(format!(
                                    "Failed to access clipboard: {}",
                                    e
                                )));
                            }
                        }
                    }
                }
                (true, toast)
            }
            _ => (false, None),
        }
    }
}
