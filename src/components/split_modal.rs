use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};
use tui_textarea::TextArea;

#[derive(Debug, Clone, thiserror::Error)]
pub enum SplitModalErr {
    #[error("No split count provided")]
    EmptyCount,
    #[error("Invalid split count: {0}")]
    InvalidCount(String),
    #[error("Split count must be greater than 1")]
    CountTooSmall,
    #[error("Split count too large (max 100)")]
    CountTooLarge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SplitMode {
    EqualSplit,
    CustomMix,
}

pub struct SplitModal {
    split_count_input: TextArea<'static>,
    small_count_input: TextArea<'static>,
    medium_count_input: TextArea<'static>,
    large_count_input: TextArea<'static>,
    active_input: usize,
    show_modal: bool,
    mode: SplitMode,
    mode_list_state: ListState,
    use_change_addresses: bool,
}

impl Default for SplitModal {
    fn default() -> Self {
        let mut split_count_input = TextArea::default();
        let mut small_count_input = TextArea::default();
        let mut medium_count_input = TextArea::default();
        let mut large_count_input = TextArea::default();

        // Configure inputs
        split_count_input.set_placeholder_text("Number of outputs (2-100)");
        split_count_input.set_cursor_line_style(Style::default());

        small_count_input.set_placeholder_text("Small UTXOs (0.001 BTC each)");
        small_count_input.set_cursor_line_style(Style::default());

        medium_count_input.set_placeholder_text("Medium UTXOs (0.01 BTC each)");
        medium_count_input.set_cursor_line_style(Style::default());

        large_count_input.set_placeholder_text("Large UTXOs (0.1 BTC each)");
        large_count_input.set_cursor_line_style(Style::default());

        let mut mode_list_state = ListState::default();
        mode_list_state.select(Some(0));

        Self {
            split_count_input,
            small_count_input,
            medium_count_input,
            large_count_input,
            active_input: 0,
            show_modal: false,
            mode: SplitMode::EqualSplit,
            mode_list_state,
            use_change_addresses: true,
        }
    }
}

impl SplitModal {
    pub fn visible(&self) -> bool {
        self.show_modal
    }

    pub fn toggle(&mut self) {
        self.show_modal = !self.show_modal;
        if self.show_modal {
            self.reset();
        }
    }

    pub fn reset(&mut self) {
        self.split_count_input = TextArea::default();
        self.small_count_input = TextArea::default();
        self.medium_count_input = TextArea::default();
        self.large_count_input = TextArea::default();

        self.split_count_input
            .set_placeholder_text("Number of outputs (2-100)");
        self.small_count_input
            .set_placeholder_text("Small UTXOs (0.001 BTC each)");
        self.medium_count_input
            .set_placeholder_text("Medium UTXOs (0.01 BTC each)");
        self.large_count_input
            .set_placeholder_text("Large UTXOs (0.1 BTC each)");

        self.active_input = 0;
        self.mode = SplitMode::EqualSplit;
        self.mode_list_state.select(Some(0));
    }

    pub fn handle_input(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.show_modal = false;
                return true;
            }
            KeyCode::Tab => {
                match self.mode {
                    SplitMode::EqualSplit => {
                        // Tab between mode selection and split count input
                        self.active_input = (self.active_input + 1) % 2;
                    }
                    SplitMode::CustomMix => {
                        // Tab between mode selection and three count inputs
                        self.active_input = (self.active_input + 1) % 4;
                    }
                }
                return true;
            }
            KeyCode::Up | KeyCode::Down => {
                if self.active_input == 0 {
                    // Handle mode selection
                    let selected = self.mode_list_state.selected().unwrap_or(0);
                    let new_selected = match key.code {
                        KeyCode::Up => {
                            if selected > 0 {
                                selected - 1
                            } else {
                                1
                            }
                        }
                        KeyCode::Down => {
                            if selected < 1 {
                                selected + 1
                            } else {
                                0
                            }
                        }
                        _ => selected,
                    };
                    self.mode_list_state.select(Some(new_selected));
                    self.mode = if new_selected == 0 {
                        SplitMode::EqualSplit
                    } else {
                        SplitMode::CustomMix
                    };
                    return true;
                }
            }
            KeyCode::Char(' ') => {
                if self.active_input == 0 {
                    self.use_change_addresses = !self.use_change_addresses;
                    return true;
                }
            }
            _ => {}
        }

        // Handle text input based on active input and mode
        if self.active_input > 0 {
            match self.mode {
                SplitMode::EqualSplit => {
                    if self.active_input == 1 {
                        self.split_count_input.input(key);
                    }
                }
                SplitMode::CustomMix => match self.active_input {
                    1 => {
                        self.small_count_input.input(key);
                    }
                    2 => {
                        self.medium_count_input.input(key);
                    }
                    3 => {
                        self.large_count_input.input(key);
                    }
                    _ => {}
                },
            }
        }

        false
    }

    pub fn get_equal_split_count(&self) -> Result<usize, SplitModalErr> {
        let text = self.split_count_input.lines()[0].trim();
        if text.is_empty() {
            return Err(SplitModalErr::EmptyCount);
        }

        let count = text
            .parse::<usize>()
            .map_err(|_| SplitModalErr::InvalidCount(text.to_string()))?;

        if count < 2 {
            return Err(SplitModalErr::CountTooSmall);
        }

        if count > 100 {
            return Err(SplitModalErr::CountTooLarge);
        }

        Ok(count)
    }

    pub fn get_custom_mix(&self) -> Result<(usize, usize, usize), SplitModalErr> {
        let small_text = self.small_count_input.lines()[0].trim();
        let medium_text = self.medium_count_input.lines()[0].trim();
        let large_text = self.large_count_input.lines()[0].trim();

        let small_count = if small_text.is_empty() {
            0
        } else {
            small_text
                .parse::<usize>()
                .map_err(|_| SplitModalErr::InvalidCount(small_text.to_string()))?
        };

        let medium_count = if medium_text.is_empty() {
            0
        } else {
            medium_text
                .parse::<usize>()
                .map_err(|_| SplitModalErr::InvalidCount(medium_text.to_string()))?
        };

        let large_count = if large_text.is_empty() {
            0
        } else {
            large_text
                .parse::<usize>()
                .map_err(|_| SplitModalErr::InvalidCount(large_text.to_string()))?
        };

        if small_count + medium_count + large_count == 0 {
            return Err(SplitModalErr::EmptyCount);
        }

        Ok((small_count, medium_count, large_count))
    }

    pub fn get_mode(&self) -> &SplitMode {
        &self.mode
    }

    pub fn use_change_addresses(&self) -> bool {
        self.use_change_addresses
    }

    pub fn render(&mut self, f: &mut Frame) {
        if !self.show_modal {
            return;
        }

        let size = f.size();
        let modal_area = Rect {
            x: size.width / 8,
            y: size.height / 8,
            width: (size.width * 3) / 4,
            height: (size.height * 3) / 4,
        };

        // Clear the background
        f.render_widget(Clear, modal_area);

        // Main modal block
        let modal_block = Block::default()
            .title("Split UTXOs")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(Color::Black));

        f.render_widget(modal_block, modal_area);

        let inner_area = modal_area.inner(&Margin {
            vertical: 1,
            horizontal: 2,
        });

        let chunks = Layout::vertical([
            Constraint::Length(4), // Mode selection
            Constraint::Length(1), // Address type selection
            Constraint::Min(5),    // Input fields
            Constraint::Length(3), // Help text
        ])
        .split(inner_area);

        // Mode selection
        let mode_items = vec![
            ListItem::new("Equal Split - Split largest UTXO into equal parts"),
            ListItem::new("Custom Mix - Create small, medium, and large UTXOs"),
        ];

        let mode_list = List::new(mode_items)
            .block(
                Block::default()
                    .title("Split Mode")
                    .borders(Borders::ALL)
                    .border_style(if self.active_input == 0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::Gray)
                    }),
            )
            .style(Style::default().fg(Color::White))
            .highlight_style(
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");

        f.render_stateful_widget(mode_list, chunks[0], &mut self.mode_list_state);

        // Address type selection
        let address_type_text = format!(
            "[{}] Use change addresses (internal) instead of external addresses",
            if self.use_change_addresses { "x" } else { " " }
        );
        let address_type_paragraph =
            Paragraph::new(address_type_text).style(if self.active_input == 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            });
        f.render_widget(address_type_paragraph, chunks[1]);

        // Input fields based on mode
        match self.mode {
            SplitMode::EqualSplit => {
                let input_area = chunks[2];
                self.split_count_input.set_block(
                    Block::default()
                        .title("Number of Equal Outputs")
                        .borders(Borders::ALL)
                        .border_style(if self.active_input == 1 {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::Gray)
                        }),
                );
                let split_count_widget = self.split_count_input.clone();
                f.render_widget(split_count_widget.widget(), input_area);
            }
            SplitMode::CustomMix => {
                let input_chunks = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                ])
                .split(chunks[2]);

                self.small_count_input.set_block(
                    Block::default()
                        .title("Small UTXOs (0.001 BTC each)")
                        .borders(Borders::ALL)
                        .border_style(if self.active_input == 1 {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::Gray)
                        }),
                );
                let small_widget = self.small_count_input.clone();
                f.render_widget(small_widget.widget(), input_chunks[0]);

                self.medium_count_input.set_block(
                    Block::default()
                        .title("Medium UTXOs (0.01 BTC each)")
                        .borders(Borders::ALL)
                        .border_style(if self.active_input == 2 {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::Gray)
                        }),
                );
                let medium_widget = self.medium_count_input.clone();
                f.render_widget(medium_widget.widget(), input_chunks[1]);

                self.large_count_input.set_block(
                    Block::default()
                        .title("Large UTXOs (0.1 BTC each)")
                        .borders(Borders::ALL)
                        .border_style(if self.active_input == 3 {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::Gray)
                        }),
                );
                let large_widget = self.large_count_input.clone();
                f.render_widget(large_widget.widget(), input_chunks[2]);
            }
        }

        let help_text = "↑↓: Navigate modes | Tab: Switch fields | Space: Toggle address type | Enter: Split | Esc: Cancel";
        let help_paragraph = Paragraph::new(help_text)
            .block(Block::default().title("Help").borders(Borders::ALL))
            .style(Style::default().fg(Color::Cyan));
        f.render_widget(help_paragraph, chunks[3]);
    }
}
