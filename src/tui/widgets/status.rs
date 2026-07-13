//! Status bar widget - shows application status at the bottom of the TUI

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Status bar widget
pub struct StatusWidget {
    /// Current status message
    status: String,
}

impl StatusWidget {
    /// Create a new status widget
    pub fn new() -> Self {
        Self {
            status: "Ready".to_string(),
        }
    }

    /// Set the status message
    pub fn set_status(&mut self, status: &str) {
        self.status = status.to_string();
    }

    /// Get the current status
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Render the status bar
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width < 5 {
            return;
        }

        // Truncate status to fit
        let max_width = area.width.saturating_sub(2) as usize;
        let display = if self.status.len() > max_width {
            format!("{}…", &self.status[..max_width.saturating_sub(1)])
        } else {
            self.status.clone()
        };

        let line = Line::from(Span::styled(
            format!(" {}", display),
            Style::default()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ));

        let paragraph = Paragraph::new(line).style(Style::default().bg(Color::Blue));
        frame.render_widget(paragraph, area);
    }
}

impl Default for StatusWidget {
    fn default() -> Self {
        Self::new()
    }
}
