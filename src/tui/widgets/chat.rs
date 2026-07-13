//! Chat widget - displays the conversation history
//!
//! This widget renders messages from the user, assistant, tools, and system
//! in a scrollable area with color-coded formatting.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

/// A single entry in the chat display
#[derive(Debug, Clone)]
pub enum ChatEntry {
    /// A user message
    UserMessage(String),
    /// An assistant message (possibly partial/streaming)
    AssistantMessage(String),
    /// A tool call
    ToolCall {
        /// Unique tool-use ID from the LLM (used to correlate start+end)
        id: String,
        name: String,
        args: String,
        success: Option<bool>,
        result_message: Option<String>,
    },
    /// A thinking block
    Thinking(String),
    /// A system message (status, errors, etc.)
    System(String),
    /// A separator
    Separator,
}

/// Chat widget that displays the conversation
pub struct ChatWidget {
    /// All entries in the chat
    pub entries: Vec<ChatEntry>,
    /// Scroll offset (0 = top, higher = more scrolled)
    scroll_offset: usize,
    /// Whether auto-scroll is enabled
    auto_scroll: bool,
    /// Maximum number of entries to keep
    max_entries: usize,
}

impl ChatWidget {
    /// Create a new chat widget
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(100),
            scroll_offset: 0,
            auto_scroll: true,
            max_entries: 1000,
        }
    }

    /// Add a user message
    pub fn add_message(&mut self, role: &str, content: &str) {
        match role {
            "user" => {
                self.entries.push(ChatEntry::UserMessage(content.to_string()));
            }
            "assistant" | "Assistant" => {
                self.entries.push(ChatEntry::AssistantMessage(content.to_string()));
            }
            "system" => {
                self.entries.push(ChatEntry::System(content.to_string()));
            }
            _ => {
                self.entries.push(ChatEntry::System(format!("[{}] {}", role, content)));
            }
        }
        self.trim_entries();
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Add a system message
    pub fn add_system_message(&mut self, content: &str) {
        self.entries.push(ChatEntry::System(content.to_string()));
        self.trim_entries();
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Add a tool call entry
    pub fn add_tool_call(&mut self, id: &str, tool_name: &str, args: &str) {
        self.entries.push(ChatEntry::ToolCall {
            id: id.to_string(),
            name: tool_name.to_string(),
            args: args.to_string(),
            success: None,
            result_message: None,
        });
        self.trim_entries();
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Update the tool call matching `id` with the execution result.
    /// Falls back to the last tool call if no ID matches (backward compat).
    pub fn update_tool_result(&mut self, tool_id: &str, is_success: bool, message: &str) {
        // Try to find the tool call by ID (search in reverse — most recent first)
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::ToolCall {
                ref id,
                ref mut success,
                ref mut result_message,
                ..
            } = entry
            {
                if id.as_str() == tool_id {
                    *success = Some(is_success);
                    *result_message = Some(message.to_string());
                    self.scroll_to_bottom();
                    return;
                }
            }
        }
        // Fallback: update the last tool call
        if let Some(entry) = self.entries.last_mut() {
            if let ChatEntry::ToolCall {
                ref mut success,
                ref mut result_message,
                ..
            } = entry
            {
                *success = Some(is_success);
                *result_message = Some(message.to_string());
                self.scroll_to_bottom();
            }
        }
    }

    /// Add a thinking block
    pub fn add_thinking(&mut self, content: &str) {
        self.entries.push(ChatEntry::Thinking(content.to_string()));
        self.trim_entries();
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Append text to the last assistant message (for streaming)
    pub fn append_to_last_text(&mut self, text: &str) {
        if let Some(entry) = self.entries.last_mut() {
            if let ChatEntry::AssistantMessage(ref mut content) = entry {
                content.push_str(text);
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
                return;
            }
        }
        // If no assistant message exists, create one
        self.entries.push(ChatEntry::AssistantMessage(text.to_string()));
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Append text to the last thinking entry
    pub fn append_to_last_thinking(&mut self, text: &str) {
        if let Some(entry) = self.entries.last_mut() {
            if let ChatEntry::Thinking(ref mut content) = entry {
                content.push_str(text);
                return;
            }
        }
        self.entries.push(ChatEntry::Thinking(text.to_string()));
    }

    /// Clear all entries
    pub fn clear(&mut self) {
        self.entries.clear();
        self.scroll_offset = 0;
    }

    /// Add a separator
    pub fn add_separator(&mut self) {
        self.entries.push(ChatEntry::Separator);
        self.trim_entries();
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Scroll up by one line
    pub fn scroll_up(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
        self.auto_scroll = false;
    }

    /// Scroll down by one line
    pub fn scroll_down(&mut self) {
        self.scroll_offset += 1;
        self.auto_scroll = false;
    }

    /// Scroll to the bottom
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.auto_scroll = true;
    }

    /// Toggle auto-scroll
    pub fn toggle_auto_scroll(&mut self) {
        self.auto_scroll = !self.auto_scroll;
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Trim entries to max_entries
    fn trim_entries(&mut self) {
        while self.entries.len() > self.max_entries {
            self.entries.remove(0);
        }
    }

    /// Get the total number of entries
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Render the chat widget into the given area
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width < 10 || area.height < 3 {
            return;
        }

        // Build the text lines
        let mut lines: Vec<Line> = Vec::new();
        let inner_width = area.width.saturating_sub(2) as usize; // Account for borders

        for entry in &self.entries {
            match entry {
                ChatEntry::UserMessage(text) => {
                    // User messages in cyan
                    let wrapped = textwrap::wrap(text, inner_width.saturating_sub(2));
                    let wrapped_refs: Vec<&str> = wrapped.iter().map(|s| s.as_ref()).collect();
                    if let Some(first) = wrapped_refs.first() {
                        lines.push(Line::from(vec![
                            Span::styled("▶ ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                            Span::styled(first.to_string(), Style::default().fg(Color::Cyan)),
                        ]));
                    }
                    for line_text in wrapped_refs.iter().skip(1) {
                        lines.push(Line::from(Span::styled(
                            format!("  {}", line_text),
                            Style::default().fg(Color::Cyan),
                        )));
                    }
                }
                ChatEntry::AssistantMessage(text) => {
                    // Assistant messages in green
                    let wrapped = textwrap::wrap(text, inner_width.saturating_sub(2));
                    let wrapped_refs: Vec<&str> = wrapped.iter().map(|s| s.as_ref()).collect();
                    if let Some(first) = wrapped_refs.first() {
                        lines.push(Line::from(vec![
                            Span::styled("◆ ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                            Span::styled(first.to_string(), Style::default().fg(Color::Green)),
                        ]));
                    }
                    for line_text in wrapped_refs.iter().skip(1) {
                        lines.push(Line::from(Span::styled(
                            format!("  {}", line_text),
                            Style::default().fg(Color::Green),
                        )));
                    }
                }
                ChatEntry::ToolCall {
                    name,
                    args,
                    success,
                    ..
                } => {
                    // Tool calls — matches old CLI style:
                    //   ⚡ ToolName args     (in progress)
                    //   ✓ ToolName args     (completed)
                    //   ✗ ToolName args     (failed)
                    // Status symbol alone indicates outcome (no extra result line).
                    let status = match success {
                        Some(true) => "✓",
                        Some(false) => "✗",
                        None => "⚡",
                    };
                    let status_color = match success {
                        Some(true) => Color::Green,
                        Some(false) => Color::Red,
                        None => Color::Yellow,
                    };
                    let tool_line = if args.is_empty() {
                        format!("{} {}", status, name)
                    } else {
                        format!("{} {}  {}", status, name, args)
                    };
                    lines.push(Line::from(Span::styled(
                        tool_line,
                        Style::default().fg(status_color),
                    )));
                }
                ChatEntry::Thinking(text) => {
                    // Thinking in dim italic
                    let wrapped = textwrap::wrap(text, inner_width.saturating_sub(2));
                    let wrapped_refs: Vec<&str> = wrapped.iter().map(|s| s.as_ref()).collect();
                    for line_text in &wrapped_refs {
                        lines.push(Line::from(Span::styled(
                            format!("💭 {}", line_text),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        )));
                    }
                }
                ChatEntry::System(text) => {
                    // System messages in yellow
                    let wrapped = textwrap::wrap(text, inner_width.saturating_sub(2));
                    let wrapped_refs: Vec<&str> = wrapped.iter().map(|s| s.as_ref()).collect();
                    for line_text in &wrapped_refs {
                        lines.push(Line::from(Span::styled(
                            format!("ℹ {}", line_text),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                }
                ChatEntry::Separator => {
                    let sep = "─".repeat(inner_width.min(60));
                    lines.push(Line::from(Span::styled(
                        sep,
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }

        // If no entries, show a welcome message
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "Welcome! Type a message to start the conversation.",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            )));
        }

        // Apply scroll offset
        let visible_lines = area.height.saturating_sub(2) as usize;
        let total_lines = lines.len();
        let max_scroll = total_lines.saturating_sub(visible_lines);
        let scroll = if self.auto_scroll {
            max_scroll
        } else {
            self.scroll_offset.min(max_scroll)
        };

        // Get the visible slice
        let start = scroll.min(total_lines.saturating_sub(1));
        let end = (start + visible_lines).min(total_lines);
        let visible: Vec<Line> = lines[start..end].to_vec();

        // Create the paragraph widget
        let paragraph = Paragraph::new(visible)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(" Conversation ")
                    .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            )
            .wrap(Wrap { trim: true });

        frame.render_widget(paragraph, area);
    }
}

impl Default for ChatWidget {
    fn default() -> Self {
        Self::new()
    }
}
