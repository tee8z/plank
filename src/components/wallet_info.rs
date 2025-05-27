use ratatui::prelude::*;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;

use crate::utils::format_amount;
use crate::wallet::AppWallet;

pub struct WalletInfo {
    name: String,
    wallet: AppWallet,
}

impl WalletInfo {
    pub fn new(name: impl ToString, wallet: AppWallet) -> Self {
        Self {
            name: name.to_string(),
            wallet,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        // Create a centered layout for the wallet info
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(4), // 3 lines of text
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
            Span::styled(&self.name, Style::default().bold()),
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

        // Syncing
        let status = self.wallet.get_sync_status();
        let syncing_text = if status.is_syncing {
            Span::styled("Syncing", Style::default().fg(Color::Rgb(242, 140, 40)))
        } else {
            Span::from("Synced")
        };
        let syncing_text = Line::from(syncing_text);

        // Create a single paragraph with all lines
        let text = Text::from(vec![name_text, balance_text, pending_text, syncing_text]);
        let info = Paragraph::new(text).centered();

        f.render_widget(info, container);
    }
}
