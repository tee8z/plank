use std::time::{Duration, Instant};

use ratatui::{style::Style, text::Span};

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
