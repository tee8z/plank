use std::str::FromStr;

use bdk_wallet::bitcoin::{Address, Network};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use tui_textarea::TextArea;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ModalErr {
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
    pub fn toggle(&mut self) {
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

    pub fn handle_input(&mut self, input: KeyEvent) -> bool {
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
    pub fn get_form_data(&self) -> Result<(Address, u64), ModalErr> {
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

    pub fn render(&mut self, f: &mut Frame) {
        if !self.show_modal {
            return;
        }

        // Create a centered area for the modal
        let area = centered_rect(60, 20, f.size());
        self.render_modal(f, area);
    }

    pub fn visible(&self) -> bool {
        self.show_modal
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.show_modal = visible;
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
