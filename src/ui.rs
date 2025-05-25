use anyhow::Result;
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

use std::time::{Duration, Instant};

pub struct App {
    pub should_quit: bool,
    wallet: AppWallet,
    config: Config,
}

impl App {
    pub fn new(wallet: AppWallet, config: Config) -> Self {
        Self {
            should_quit: false,
            wallet,
            config,
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

    fn draw(&self, f: &mut Frame) {
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
        let transactions_area = Layout::vertical([
            Constraint::Length(1), // Title
            Constraint::Min(1),    // Transactions list
        ])
        .split(area);

        let transactions_title = Paragraph::new(Span::styled(
            "Transactions",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        f.render_widget(transactions_title, transactions_area[0]);

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

        f.render_widget(table, transactions_area[1]);
    }

    fn handle_events(&mut self) -> Result<()> {
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => {
                        self.should_quit = true;
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        self.current_tab = self.current_tab.previous();
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        self.current_tab = self.current_tab.next();
                    }
                    KeyCode::Char('1') => {
                        self.current_tab = Tab::Send;
                    }
                    KeyCode::Char('2') => {
                        self.current_tab = Tab::Receive;
                    }
                    KeyCode::Char('3') => {
                        self.current_tab = Tab::Transactions;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}
