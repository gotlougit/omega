//! Input widget - text input bar at the bottom of the TUI
//!
//! Provides a styled input field with cursor and placeholder text.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Input widget for user text entry
pub struct InputWidget {
    /// The current input text
    text: String,
    /// Cursor position within the text
    cursor_pos: usize,
    /// Placeholder text when input is empty
    placeholder: String,
}

impl InputWidget {
    /// Create a new input widget
    pub fn new() -> Self {
        Self {
            text: String::with_capacity(256),
            cursor_pos: 0,
            placeholder: "Type a message...".to_string(),
        }
    }

    /// Get the current input text
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Clear the input
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_pos = 0;
    }

    /// Insert a character at cursor position
    pub fn insert_char(&mut self, c: char) {
        if self.cursor_pos <= self.text.len() {
            self.text.insert(self.cursor_pos, c);
            self.cursor_pos += 1;
        }
    }

    /// Delete the character before cursor (backspace)
    pub fn delete_char(&mut self) {
        if self.cursor_pos > 0 && self.cursor_pos <= self.text.len() {
            self.text.remove(self.cursor_pos - 1);
            self.cursor_pos -= 1;
        }
    }

    /// Delete the character at cursor (delete forward)
    pub fn delete_forward(&mut self) {
        if self.cursor_pos < self.text.len() {
            self.text.remove(self.cursor_pos);
        }
    }

    /// Move cursor left
    pub fn move_cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
        }
    }

    /// Move cursor right
    pub fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.text.len() {
            self.cursor_pos += 1;
        }
    }

    /// Move cursor to the beginning
    pub fn move_cursor_home(&mut self) {
        self.cursor_pos = 0;
    }

    /// Move cursor to the end
    pub fn move_cursor_end(&mut self) {
        self.cursor_pos = self.text.len();
    }

    /// Set placeholder text
    pub fn set_placeholder(&mut self, placeholder: &str) {
        self.placeholder = placeholder.to_string();
    }

    /// Get cursor position
    pub fn cursor_pos(&self) -> usize {
        self.cursor_pos
    }

    /// Render the input widget
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width < 10 {
            return;
        }

        // Build the display text
        let display_text = if self.text.is_empty() {
            // Show placeholder in dim style
            Line::from(Span::styled(
                &self.placeholder,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))
        } else {
            Line::from(Span::raw(&self.text))
        };

        let paragraph = Paragraph::new(display_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Input ")
                    .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            )
            .style(Style::default().fg(Color::White));

        frame.render_widget(paragraph, area);

        // Set cursor position
        // The cursor should be at the position inside the input area
        if let Some(cursor_x) = area.x.checked_add(self.cursor_pos as u16 + 1) {
            if cursor_x < area.x + area.width.saturating_sub(1) {
                frame.set_cursor(cursor_x, area.y + 1);
            }
        }
    }
}

impl Default for InputWidget {
    fn default() -> Self {
        Self::new()
    }
}
