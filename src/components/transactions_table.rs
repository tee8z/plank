use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::Text,
    widgets::{Block, Cell, Padding, Row, Table},
    Frame,
};

use crate::utils::format_signed_amount;
use crate::wallet::AppWallet;

#[derive(Debug, Clone)]
pub struct TransactionsTable {
    wallet: AppWallet,
}

impl TransactionsTable {
    pub fn new(wallet: AppWallet) -> Self {
        Self { wallet }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
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
}
