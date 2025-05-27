use ratatui::{
    prelude::*,
    widgets::{Block, Cell, Padding, Row, Table},
};

use crate::utils::format_amount;
use crate::wallet::AppWallet;

pub struct UtxosTable {
    wallet: AppWallet,
}

impl UtxosTable {
    pub fn new(wallet: AppWallet) -> Self {
        Self { wallet }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
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
}
