//! # omega — TUI client for the picrust omega services
//!
//! Terminal User Interface client that connects to the `omega-loop` daemon
//! and renders streaming responses using Ratatui.
//!
//! Connects using `OMEGA_LOOP_SOCKET_PATH` env var or defaults to
//! `/tmp/omega-loop.sock`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};
use std::process::Stdio;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind, EnableBracketedPaste, DisableBracketedPaste};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc;

use picrust::omega_loop_client::{AgentdClient, OutputChunk, ServerEvent, SessionConfig};

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

const COMMANDS: &[CommandDef] = &[
    CommandDef { name: "/new", args: "", desc: "Start a new session" },
    CommandDef { name: "/resume", args: "[id]", desc: "Resume an existing session" },
    CommandDef { name: "/model", args: "<name>", desc: "Change the LLM model" },
    CommandDef { name: "/compact", args: "", desc: "Compact the current session" },
    CommandDef { name: "/help", args: "", desc: "Show this help" },
    CommandDef { name: "/quit", args: "", desc: "Quit the program" },
];

struct CommandDef {
    name: &'static str,
    args: &'static str,
    desc: &'static str,
}

fn matching_commands(input: &str) -> Vec<(usize, &'static CommandDef)> {
    if !input.starts_with('/') || input.is_empty() {
        return Vec::new();
    }
    let lower = input.to_lowercase();
    COMMANDS.iter().enumerate()
        .filter(|(_, cmd)| cmd.name.to_lowercase().starts_with(&lower))
        .collect()
}

fn format_command(cmd: &CommandDef) -> String {
    if cmd.args.is_empty() {
        format!("{}", cmd.name)
    } else {
        format!("{} {}", cmd.name, cmd.args)
    }
}

// ---------------------------------------------------------------------------
// TUI Client State
// ---------------------------------------------------------------------------

/// Main TUI application state
struct TuiClient {
    chat_lines: Vec<Line<'static>>,
    input: String,
    cursor_pos: usize,
    session_id: String,
    status: String,
    running: bool,
    auto_scroll: bool,
    scroll_offset: usize,
    waiting_permission: bool,
    waiting_question: bool,
    permission_tool: String,
    question_request_id: String,
    questions: Vec<picrust::omega_loop_client::UserQuestionWire>,
    processing: bool,
    session_list: Vec<String>, // for /resume
    waiting_session_list: bool,
    /// Currently highlighted command index in the command preview (None = no preview).
    command_selection: Option<usize>,
    /// Maps tool-use ID → (line_index, tool_name) so we can update
    /// the correct line when the tool result arrives (handles multiple
    /// concurrent tool calls correctly) and identify the tool type.
    pending_tool_calls: HashMap<String, (usize, String)>,
    /// Index in `chat_lines` where the current streaming assistant
    /// response begins (set by first TextDelta, cleared on Done).
    pending_assistant_idx: Option<usize>,
    /// Raw accumulated text of the streaming assistant response.
    /// Re-parsed into styled markdown spans on each chunk.
    pending_assistant_raw: String,
    /// Index in `chat_lines` where the current streaming thinking block begins.
    pending_thinking_idx: Option<usize>,
    /// Raw accumulated thinking text.
    pending_thinking_raw: String,
    /// Index of the collapsed "💭 Thinking..." summary line (None if expanded).
    collapsed_thinking_line: Option<usize>,
    /// Raw text preserved for expanding a collapsed thinking block.
    collapsed_thinking_raw: String,
    /// Selection state — screen coordinates (col, row).
    selection_start: Option<(u16, u16)>,
    selection_end: Option<(u16, u16)>,
    /// The last terminal area used for selection (needed by render to convert coords).
    selection_term_area: ratatui::layout::Rect,
    /// Flash message shown temporarily at the bottom of the chat.
    flash_message: Option<(String, Instant)>,
    /// Timestamp of the first Ctrl+C press (for double-press-to-quit).
    ctrl_c_pressed: Option<Instant>,
}

impl TuiClient {
    fn new() -> Self {
        Self {
            chat_lines: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            session_id: String::new(),
            status: "Ready".to_string(),
            running: true,
            auto_scroll: true,
            scroll_offset: 0,
            waiting_permission: false,
            waiting_question: false,
            permission_tool: String::new(),
            question_request_id: String::new(),
            questions: Vec::new(),
            processing: false,
            pending_tool_calls: HashMap::new(),
            pending_assistant_idx: None,
            pending_assistant_raw: String::new(),
            pending_thinking_idx: None,
            pending_thinking_raw: String::new(),
            collapsed_thinking_line: None,
            collapsed_thinking_raw: String::new(),
            selection_start: None,
            selection_end: None,
            selection_term_area: ratatui::layout::Rect::new(0,0,80,24),
            flash_message: None,
            session_list: Vec::new(),
            waiting_session_list: false,
            command_selection: None,
            ctrl_c_pressed: None,
        }
    }

    fn add_line(&mut self, line: Line<'static>) {
        self.chat_lines.push(line);
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    fn add_system_msg(&mut self, msg: String) {
        self.add_line(Line::from(Span::styled(
            format!("ℹ {}", msg),
            Style::default().fg(Color::Yellow),
        )));
    }

    fn add_user_msg(&mut self, msg: String) {
        self.add_line(Line::from(vec![
            Span::styled("▶ ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(msg, Style::default().fg(Color::Cyan)),
        ]));
    }

    fn add_assistant_chunk(&mut self, text: &str) {
        // Accumulate raw text and re-render with markdown on each chunk
        self.pending_assistant_raw.push_str(text);
        self.flush_assistant();
    }

    /// Replace the streaming assistant lines with freshly markdown-rendered content.
    fn flush_assistant(&mut self) {
        let raw = &self.pending_assistant_raw;

        // Split at newlines so each paragraph is a separate Line
        let paragraphs: Vec<&str> = if raw.contains("\n") {
            raw.split('\n').collect()
        } else {
            vec![raw.as_str()]
        };

        // Build rendered Lines with markdown styling
        let mut rendered: Vec<Line<'static>> = Vec::with_capacity(paragraphs.len());
        let mut in_code_block = false;
        for (i, para) in paragraphs.iter().enumerate() {
            let base = Style::default().fg(Color::Green);
            let mut spans = Vec::new();

            // --- Code block fence detection ---
            let code_bg = Color::Rgb(40, 40, 50);
            if para.trim_start().starts_with("```") {
                in_code_block = !in_code_block;
                // Skip fence lines entirely — no ugly markers
                continue;
            }

            if in_code_block {
                // Code lines — grey background, use zero-width no-break space for empty lines
                // so trim: true doesn't strip them (no-break space is not ASCII whitespace)
                let display = if para.trim().is_empty() { "\u{00a0}" } else { para };
                rendered.push(Line::from(Span::styled(
                    display.to_string(),
                    Style::default()
                        .fg(Color::White)
                        .bg(code_bg),
                )));
                continue;
            }

            // --- Heading detection ---
            if let Some(heading_level) = heading_level(para) {
                let heading_style = match heading_level {
                    1 => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    2 => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    _ => Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
                };
                let heading_text = para.trim_start_matches(|c: char| c == '#').trim();
                // Prefix with a marker
                rendered.push(Line::from(Span::styled(
                    format!("▎ {} ", heading_text),
                    heading_style,
                )));
                continue;
            }

            if i == 0 {
                spans.push(Span::styled(
                    "◆ ",
                    base.add_modifier(Modifier::BOLD),
                ));
            } else if para.is_empty() {
                rendered.push(Line::from(""));
                continue;
            } else {
                spans.push(Span::styled(
                    "  ",
                    base,
                ));
            }
            if is_table_row(para) {
                spans.extend(render_table_row(para, base));
            } else if is_table_separator(para) {
                spans.push(Span::styled(
                    para.replace('|', "┼").replace('-', "─"),
                    base.fg(Color::DarkGray),
                ));
            } else {
                spans.extend(parse_inline_markdown(para, base));
            }
            rendered.push(Line::from(spans));
        }

        // Replace the old assistant entries with the freshly rendered ones
        if let Some(idx) = self.pending_assistant_idx {
            // Remove old lines from idx onward (all the previous assistant lines)
            let remove_count = self.chat_lines.len().saturating_sub(idx);
            if remove_count > 0 {
                self.chat_lines.truncate(idx);
            }
            // Add new rendered lines
            for line in rendered {
                self.chat_lines.push(line);
            }
        } else {
            // First chunk — record start index and add lines
            self.pending_assistant_idx = Some(self.chat_lines.len());
            for line in rendered {
                self.chat_lines.push(line);
            }
        }

        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Finalise the current streaming assistant response.
    fn finish_assistant(&mut self) {
        self.pending_assistant_idx = None;
        self.pending_assistant_raw.clear();
    }

    /// Stream a thinking delta.
    ///
    /// - If a collapsed block exists, we auto-expand it first (new thinking block).
    /// - If the current block is collapsed, just accumulate silently.
    /// - If the current block is expanded, render lines inline.
    fn add_thinking_chunk(&mut self, text: &str) {
        // Detect a new thinking block while previous one was still collapsed
        if self.collapsed_thinking_line.is_some() && self.pending_thinking_raw.is_empty() {
            // ThinkingComplete was already processed (raw was moved to collapsed_thinking_raw).
            // A new thinking block is starting — auto-expand the old one first.
            self.expand_thinking();
            // Now start fresh for the new block
            self.pending_thinking_raw.push_str(text);
            let idx = self.chat_lines.len();
            self.collapsed_thinking_line = Some(idx);
            self.chat_lines.push(Line::from(Span::styled(
                "💭 Thinking...",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC | Modifier::BOLD),
            )));
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
            return;
        }

        let start_new = self.pending_thinking_raw.is_empty()
            && self.collapsed_thinking_line.is_none()
            && self.pending_thinking_idx.is_none();

        self.pending_thinking_raw.push_str(text);

        if start_new {
            // First chunk of a new thinking block — start collapsed
            let idx = self.chat_lines.len();
            self.collapsed_thinking_line = Some(idx);
            self.chat_lines.push(Line::from(Span::styled(
                "💭 Thinking...",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC | Modifier::BOLD),
            )));
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
        } else if self.collapsed_thinking_line.is_some() {
            // Still collapsed — update summary with live line count
            let line_count = self.pending_thinking_raw.lines().count();
            if let Some(idx) = self.collapsed_thinking_line {
                let count_str = if line_count == 1 { "1 line".to_string() } else { format!("{} lines", line_count) };
                if let Some(line) = self.chat_lines.get_mut(idx) {
                    *line = Line::from(Span::styled(
                        format!("💭 Thinking...  ({})  [click to expand]", count_str),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC | Modifier::BOLD),
                    ));
                }
            }
        } else {
            // Expanded — render lines inline
            self.flush_thinking();
        }
    }

    /// Replace the streaming thinking lines with freshly split content.
    fn flush_thinking(&mut self) {
        let raw = &self.pending_thinking_raw;
        let lines: Vec<&str> = raw.split('\n').collect();

        let base = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        let mut rendered: Vec<Line<'static>> = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() && i > 0 {
                rendered.push(Line::from(""));
                continue;
            }
            let prefix = if i == 0 { "💭 " } else { "   " };
            let prefix_style = if i == 0 {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            };
            rendered.push(Line::from(Span::styled(
                format!("{}{}", prefix, line),
                prefix_style,
            )));
        }

        if let Some(idx) = self.pending_thinking_idx {
            let remove_count = self.chat_lines.len().saturating_sub(idx);
            if remove_count > 0 {
                self.chat_lines.truncate(idx);
            }
            for line in rendered {
                self.chat_lines.push(line);
            }
        } else {
            self.pending_thinking_idx = Some(self.chat_lines.len());
            for line in rendered {
                self.chat_lines.push(line);
            }
        }

        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Finalise the current streaming thinking block.
    fn finish_thinking(&mut self) {
        if let Some(idx) = self.collapsed_thinking_line {
            // Still collapsed — move raw text to the preservation buffer
            self.collapsed_thinking_raw = std::mem::take(&mut self.pending_thinking_raw);
            let line_count = self.collapsed_thinking_raw.lines().count();
            let summary = if line_count == 1 {
                "💭 Thinking...  (1 line)  [click to expand]".to_string()
            } else {
                format!("💭 Thinking...  ({} lines)  [click to expand]", line_count)
            };
            if let Some(line) = self.chat_lines.get_mut(idx) {
                *line = Line::from(Span::styled(
                    summary,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC | Modifier::BOLD),
                ));
            }
        } else {
            // Was already expanded — clean up
            self.pending_thinking_idx = None;
            self.pending_thinking_raw.clear();
        }
    }

    /// Expand a collapsed thinking block, replacing the summary with full content.
    fn expand_thinking(&mut self) {
        let collapsed_idx = match self.collapsed_thinking_line {
            Some(i) => i,
            None => return,
        };
        self.collapsed_thinking_line = None;
        // Restore raw text from the preservation buffer
        self.pending_thinking_raw = std::mem::take(&mut self.collapsed_thinking_raw);
        self.pending_thinking_idx = Some(collapsed_idx);
        self.flush_thinking();
    }

    fn add_tool_call(&mut self, id: &str, tool_name: &str, args: &str) {
        // Indented with two spaces to match old CLI style.
        let line = if args.is_empty() {
            format!("  ⚡ {}", tool_name)
        } else {
            format!("  ⚡ {}  {}", tool_name, args)
        };
        let idx = self.chat_lines.len();
        self.chat_lines.push(Line::from(Span::styled(
            line,
            Style::default().fg(Color::Yellow),
        )));
        self.pending_tool_calls.insert(id.to_string(), (idx, tool_name.to_string()));
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    fn add_tool_result(&mut self, id: &str, success: bool, msg: &str) {
        let color = if success { Color::Green } else { Color::Red };
        let symbol = if success { "✓" } else { "✗" };

        // Look up the (line_index, tool_name) for this tool ID
        let tool_name: Option<String> = self.pending_tool_calls.get(id).map(|e| e.1.clone());

        if let Some(idx) = self.pending_tool_calls.get(id).map(|e| e.0) {
            if let Some(line) = self.chat_lines.get_mut(idx) {
                if let Some(first) = line.spans.first_mut() {
                    let old = first.content.as_ref().to_string();
                    first.content = Cow::Owned(old.replacen('⚡', symbol, 1));
                    first.style = Style::default().fg(color);
                }
            }
            self.pending_tool_calls.remove(id);
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
        } else {
            // Fallback: try the last line (backward compat)
            if let Some(last) = self.chat_lines.last_mut() {
                let first_span_content: Option<&str> =
                    last.spans.first().map(|s| s.content.as_ref());
                if first_span_content.map_or(false, |c| c.contains('⚡')) {
                    let old = last.spans[0].content.as_ref().to_string();
                    last.spans[0] = Span::styled(
                        old.replacen('⚡', symbol, 1),
                        Style::default().fg(color),
                    );
                    if self.auto_scroll {
                        self.scroll_to_bottom();
                    }
                    return;
                }
            }
            // Last resort: add as separate line
            self.add_line(Line::from(Span::styled(
                format!("  {}", symbol),
                Style::default().fg(color),
            )));
        }

        // --- TransferDiff: save the patch to PWD ---
        if success && tool_name.as_deref() == Some("TransferDiff") && !msg.is_empty() {
            self.save_transfer_diff(msg);
        }
    }

    /// Save a patch file received via TransferDiff to the user's PWD.
    fn save_transfer_diff(&mut self, patch: &str) {
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        // Sanitize session ID for use in a filename (replace non-alphanumeric chars)
        let safe_session: String = self
            .session_id
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        let file_name = format!("transfer-{}-{}.patch", safe_session, timestamp);

        match std::fs::write(&file_name, patch) {
            Ok(_) => {
                let abs_path = std::env::current_dir()
                    .unwrap_or_default()
                    .join(&file_name);
                self.add_system_msg(format!(
                    "⬆️ Patch saved to {}",
                    abs_path.display()
                ));
                self.add_system_msg(format!(
                    "   ({} KB, {} lines)",
                    patch.len() / 1024,
                    patch.lines().count(),
                ));
            }
            Err(e) => {
                self.add_system_msg(format!(
                    "❌ Failed to save patch to '{}': {}",
                    file_name, e
                ));
            }
        }
    }

    fn add_separator(&mut self) {
        self.add_line(Line::from(Span::styled(
            "─".repeat(60),
            Style::default().fg(Color::DarkGray),
        )));
    }

    fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
        self.scroll_offset = 0;
    }

    /// Scroll up by one visible page (see OLDER content).
    fn scroll_up_page(&mut self, visible_lines: usize) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_add(visible_lines.saturating_sub(2));
    }

    /// Scroll down by one visible page (see NEWER content).
    fn scroll_down_page(&mut self, visible_lines: usize) {
        if self.scroll_offset == 0 {
            return; // already at bottom
        }
        let amount = visible_lines.saturating_sub(2);
        if self.scroll_offset <= amount {
            self.scroll_to_bottom();
        } else {
            self.auto_scroll = false;
            self.scroll_offset = self.scroll_offset.saturating_sub(amount);
        }
    }

    /// Scroll up by one line (see OLDER content).
    fn scroll_up_line(&mut self) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    /// Scroll down by one line (see NEWER content).
    fn scroll_down_line(&mut self) {
        if self.scroll_offset == 0 {
            return; // already at bottom
        }
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
        if self.scroll_offset == 0 {
            self.scroll_to_bottom(); // snapped to the very bottom → re-enable auto-scroll
        }
    }

    /// Compute the chat content area from a terminal `area` (replicates layout logic).
    fn chat_area_from_size(&self, term_area: ratatui::layout::Rect) -> ratatui::layout::Rect {
        let inner_w = term_area.width.saturating_sub(2) as usize;
        let text_lines = if self.input.is_empty() || inner_w == 0 {
            1
        } else {
            let chars = self.input.chars().count();
            ((chars + inner_w - 1) / inner_w).max(1).min(10)
        };
        let input_h = 2 + text_lines as u16;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(input_h),
            ])
            .split(term_area);
        chunks[0]
    }

    /// Convert screen coordinate to content (line_idx, char_offset) relative to chat_lines.
    fn screen_to_content(&self, col: u16, row: u16, term_area: ratatui::layout::Rect) -> Option<(usize, usize)> {
        let chat = self.chat_area_from_size(term_area);
        let inner_x = chat.x + 1;
        let inner_y = chat.y + 1;
        if row < inner_y || col < inner_x { return None; }
        let total_lines = self.chat_lines.len();
        let visible_lines = chat.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible_lines);
        let scroll_start = if self.auto_scroll {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_offset.min(max_scroll))
        };
        let line_idx = scroll_start + (row - inner_y) as usize;
        if line_idx >= total_lines { return None; }
        let col_offset = (col - inner_x) as usize;
        Some((line_idx, col_offset))
    }

    /// Start a mouse-based text selection.
    fn start_selection(&mut self, col: u16, row: u16, term_area: ratatui::layout::Rect) {
        if self.screen_to_content(col, row, term_area).is_some() {
            self.selection_start = Some((col, row));
            self.selection_end = Some((col, row));
            self.selection_term_area = term_area;
        }
    }

    /// Update the selection end point during a drag.
    fn update_selection(&mut self, col: u16, row: u16) {
        if self.selection_start.is_some() {
            self.selection_end = Some((col, row));
            // We can't update sel_b here because we don't have term_area
            // (it'll be corrected on mouse release). But for visual rendering
            // we'll recompute from screen coords in render().
        }
    }

    /// Finish selection, extract text, copy to clipboard, and show flash.
    fn finish_selection(&mut self, col: u16, row: u16, term_area: ratatui::layout::Rect) {
        let start = match self.selection_start {
            Some(s) => s,
            None => return,
        };
        self.selection_end = Some((col, row));
        let end = self.selection_end.unwrap();

        // Convert screen coords to (line_index, char_offset) in chat_lines
        let chat = self.chat_area_from_size(term_area);
        let inner_x = chat.x + 1;
        let inner_y = chat.y + 1;

        // Visible lines range
        let total_lines = self.chat_lines.len();
        let visible_lines = chat.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible_lines);
        let scroll_start = if self.auto_scroll {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_offset.min(max_scroll))
        };

        let to_line_col = |sc: (u16, u16)| -> Option<(usize, usize)> {
            let (c, r) = sc;
            if r < inner_y || r >= inner_y + visible_lines as u16 { return None; }
            if c < inner_x { return None; }
            let line_idx = scroll_start + (r - inner_y) as usize;
            if line_idx >= total_lines { return None; }
            let col_offset = (c - inner_x) as usize;
            Some((line_idx, col_offset))
        };

        let a = to_line_col(start);
        let b = to_line_col(end);
        let (a, b) = match (a, b) {
            (Some(a), Some(b)) => (a, b),
            _ => { self.selection_start = None; self.selection_end = None; return; }
        };
        // Normalise so a <= b
        let (a, b) = if a > b { (b, a) } else { (a, b) };

        // Helper: extract substring by character offsets (safe with multi-byte chars)
        let chars_substr = |s: &str, start_char: usize, end_char: usize| -> String {
            s.chars().skip(start_char).take(end_char.saturating_sub(start_char)).collect()
        };
        let chars_from = |s: &str, start_char: usize| -> String {
            s.chars().skip(start_char).collect()
        };
        let chars_upto = |s: &str, end_char: usize| -> String {
            s.chars().take(end_char).collect()
        };

        // Extract text between (a.0, a.1) and (b.0, b.1)
        let selected = if a.0 == b.0 {
            // Same line
            let line = &self.chat_lines[a.0];
            let text: String = line.spans.iter().flat_map(|s| s.content.chars()).collect();
            let text_len = text.chars().count();
            if a.1 < text_len && b.1 <= text_len && a.1 <= b.1 {
                chars_substr(&text, a.1, b.1)
            } else {
                text
            }
        } else {
            // Multiple lines
            let mut parts = Vec::new();
            for li in a.0..=b.0 {
                let line = &self.chat_lines[li];
                let text: String = line.spans.iter().flat_map(|s| s.content.chars()).collect();
                let text_len = text.chars().count();
                if li == a.0 {
                    if a.1 < text_len {
                        parts.push(chars_from(&text, a.1));
                    }
                } else if li == b.0 {
                    if b.1 <= text_len && b.1 > 0 {
                        parts.push(chars_upto(&text, b.1));
                    } else {
                        parts.push(text);
                    }
                } else {
                    parts.push(text);
                }
            }
            parts.join("\n")
        };

        self.selection_start = None;
        self.selection_end = None;

        if !selected.is_empty() {
            let trimmed = selected.trim().to_string();
            if !trimmed.is_empty() {
                copy_to_clipboard(&trimmed);
                self.flash_message = Some(("Copied to clipboard!".to_string(), Instant::now()));
            }
        }
    }

    /// Convert character-index cursor_pos to a byte index for String ops.
    fn cursor_byte(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    /// Number of chars in input (not bytes).
    fn char_count(&self) -> usize {
        self.input.chars().count()
    }

    /// Called once per frame — handles time-based state like flash message expiry.
    fn tick(&mut self) {
        // Clear expired flash messages (show for ~5 seconds)
        if let Some((_, expiry)) = &self.flash_message {
            if expiry.elapsed() >= Duration::from_secs(5) {
                self.flash_message = None;
            }
        }
    }

    fn clear(&mut self) {
        self.chat_lines.clear();
        self.input.clear();
        self.cursor_pos = 0;
        self.finish_assistant();
        self.finish_thinking();
    }

    fn handle_chunk(&mut self, chunk: &OutputChunk) {
        match chunk {
            OutputChunk::TextDelta(text) => self.add_assistant_chunk(text),
            OutputChunk::TextComplete(_) => {
                // Text block completed — markdown is already rendered line-by-line
            }
            OutputChunk::ThinkingDelta(text) => self.add_thinking_chunk(text),
            OutputChunk::ThinkingComplete(_) => {
                self.finish_thinking();
            }
            OutputChunk::ToolStart { id, name, input, .. } => {
                self.add_tool_call(id, name, &tool_input_preview(input));
            }
            OutputChunk::ToolProgress { .. } => {
                // Tool progress output — could be rendered inline if desired
            }
            OutputChunk::ToolEnd { id, result, .. } => {
                self.add_tool_result(id, !result.is_error, &result.text);
            }
            OutputChunk::Error(e) => {
                self.finish_assistant();
                self.finish_thinking();
                self.add_system_msg(format!("❌ Error: {}", e));
            }
            OutputChunk::Done => {
                self.finish_assistant();
                self.finish_thinking();
                self.add_separator();
                self.status = "Ready".to_string();
                self.processing = false;
            }
            OutputChunk::Status(s) => {
                self.add_system_msg(s.clone());
                self.status = s.clone();
            }
            OutputChunk::PermissionRequest {
                tool_name,
                action,
                details,
                ..
            } => {
                self.add_system_msg(format!(
                    "🔐 Permission needed: {} — {} {}",
                    tool_name,
                    action,
                    details.as_deref().unwrap_or("")
                ));
            }
            OutputChunk::AskUserQuestion {
                request_id,
                questions,
            } => {
                self.waiting_question = true;
                self.question_request_id = request_id.clone();
                self.questions = questions.to_vec();
                self.add_system_msg("❓ Agent has a question:".to_string());
                for q in questions {
                    self.add_system_msg(format!("  [{}] {}", q.header, q.question));
                    for (i, opt) in q.options.iter().enumerate() {
                        self.add_system_msg(format!("    {}. {} - {}", i + 1, opt.label, opt.description));
                    }
                }
                self.status = "Select an option (number)".to_string();
            }
            _ => {}
        }
    }

    fn render(&self, frame: &mut ratatui::Frame) {
        let area = frame.size();

        // Calculate how many text lines the input will wrap to
        let input_inner_width = area.width.saturating_sub(2) as usize; // subtract borders
        let input_text_lines = if self.input.is_empty() || input_inner_width == 0 {
            1
        } else {
            let chars = self.input.chars().count();
            let lines = (chars + input_inner_width - 1) / input_inner_width; // ceil division
            lines.max(1).min(10) // at least 1, at most 10
        };
        let input_total_height = 2 + input_text_lines as u16; // borders + text lines

        // Command preview: show matching commands when input starts with /
        let cmd_matches = if self.input.starts_with('/') && !self.input.is_empty() {
            matching_commands(&self.input)
        } else {
            Vec::new()
        };
        let cmd_preview_h: u16 = if cmd_matches.is_empty() || self.processing || self.waiting_permission || self.waiting_question {
            0
        } else {
            (cmd_matches.len() as u16).min(5).saturating_add(2) // border + up to 5 items + border
        };

        let mut constraints = vec![
            Constraint::Min(1),
            Constraint::Length(1),
        ];
        if cmd_preview_h > 0 {
            constraints.push(Constraint::Length(cmd_preview_h));
        }
        constraints.push(Constraint::Length(input_total_height));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        // Chat area
        let chat_area = chunks[0];
        let visible_lines = chat_area.height.saturating_sub(2) as usize;
        let total_lines = self.chat_lines.len();

        // Compute selection range in content coordinates (for visual highlighting)
        let sel_range: Option<((usize, usize), (usize, usize))> = self.selection_start.and_then(|_| {
            let s = self.screen_to_content(
                self.selection_start?.0, self.selection_start?.1, self.selection_term_area);
            let e = self.screen_to_content(
                self.selection_end?.0, self.selection_end?.1, self.selection_term_area);
            match (s, e) {
                (Some(a), Some(b)) => {
                    if a <= b { Some((a, b)) } else { Some((b, a)) }
                }
                _ => None,
            }
        });

        // Build display lines with selection + code padding
        let code_bg = Color::Rgb(40, 40, 50);
        let inner_w = chat_area.width.saturating_sub(2) as usize;
        let display_lines: Vec<Line<'static>> = if total_lines > 0 {
            self.chat_lines.iter().enumerate().map(|(i, line)| {
                let mut line = line.clone();
                if let Some(((sel_la, sel_ca), (sel_lb, sel_cb))) = sel_range {
                    if i >= sel_la && i <= sel_lb {
                        let (sc, ec) = if i == sel_la && i == sel_lb {
                            (sel_ca.min(sel_cb), sel_ca.max(sel_cb))
                        } else if i == sel_la {
                            (sel_ca, usize::MAX)
                        } else if i == sel_lb {
                            (0, sel_cb)
                        } else {
                            (0, usize::MAX)
                        };
                        line = apply_selection_to_line(&line, sc, ec);
                    }
                }
                let is_code = line.spans.first().map_or(false, |s| s.style.bg == Some(code_bg));
                if is_code {
                    let cur: usize = line.spans.iter().flat_map(|s| s.content.chars()).count();
                    if cur < inner_w {
                        line.spans.push(Span::styled(
                            " ".repeat(inner_w - cur),
                            Style::default().bg(code_bg),
                        ));
                    }
                }
                line
            }).collect()
        } else {
            vec![Line::from(Span::styled(
                "Welcome! Type a message to start.",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ))]
        };

        // Scroll calculation — reserve 1 row for wrapping so the last line is never clipped
        let scroll_lines = visible_lines.saturating_sub(1);
        let max_scroll = display_lines.len().saturating_sub(scroll_lines);
        let scroll = if self.auto_scroll {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_offset.min(max_scroll))
        };
        let start = scroll.min(display_lines.len().saturating_sub(1));
        let end = (start + scroll_lines).min(display_lines.len());
        let visible: Vec<Line> = if start < end {
            display_lines[start..end].to_vec()
        } else if !display_lines.is_empty() {
            vec![display_lines[0].clone()]
        } else {
            vec![Line::from(Span::styled(
                "Welcome! Type a message to start.",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ))]
        };

        frame.render_widget(
            Paragraph::new(visible)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(" Conversation ")
                        .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                )
                .wrap(Wrap { trim: true }),
            chat_area,
        );

        // Status bar
        let status_area = chunks[1];
        let dl = display_lines.len();
        let scroll_label = if !self.auto_scroll && dl > scroll_lines {
            let pos = self.scroll_offset.min(max_scroll);
            format!(" [SCRL {}/{}]", pos, max_scroll)
        } else {
            String::new()
        };
        let status_with_scroll = format!("{}{}", self.status, scroll_label);
        let status_display = if status_with_scroll.len() > status_area.width.saturating_sub(2) as usize {
            let max = status_area.width.saturating_sub(3) as usize;
            format!("{}…", &status_with_scroll[..max])
        } else {
            status_with_scroll
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", status_display),
                Style::default().fg(Color::White).bg(Color::Blue).add_modifier(Modifier::BOLD),
            )))
            .style(Style::default().bg(Color::Blue)),
            status_area,
        );

        // Command preview (shown when input starts with /)
        let input_area_idx = if cmd_preview_h > 0 { 3 } else { 2 };
        if cmd_preview_h > 0 {
            let cmd_area = chunks[2];
            let sel = self.command_selection;
            let cmd_lines: Vec<Line> = cmd_matches.iter().enumerate().map(|(i, (_, cmd))| {
                let is_sel = sel == Some(i);
                let style = if is_sel {
                    Style::default().fg(Color::White).bg(Color::Rgb(60, 80, 180))
                } else {
                    Style::default().fg(Color::Cyan)
                };
                let prefix = if is_sel { "▸ " } else { "  " };
                Line::from(Span::styled(
                    format!("{}{}  — {}", prefix, format_command(cmd), cmd.desc),
                    style,
                ))
            }).collect();
            frame.render_widget(
                Paragraph::new(cmd_lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Cyan))
                            .title(" Commands ")
                            .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    )
                    .style(Style::default().fg(Color::White)),
                cmd_area,
            );
        }

        // Input bar
        let input_area = chunks[input_area_idx];

        // Build wrapped lines for the input text
        let input_lines: Vec<Line> = if self.input.is_empty() {
            vec![Line::from(Span::styled(
                "Type a message...",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ))]
        } else if input_inner_width > 0 {
            self.input
                .chars()
                .collect::<Vec<char>>()
                .chunks(input_inner_width)
                .map(|chunk| Line::from(Span::raw(chunk.iter().collect::<String>())))
                .collect()
        } else {
            vec![Line::from(Span::raw(&self.input))]
        };

        frame.render_widget(
            Paragraph::new(
                input_lines.into_iter().take(input_text_lines as usize).collect::<Vec<_>>()
            )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(" Input ")
                        .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                )
                .style(Style::default().fg(Color::White)),
            input_area,
        );

        // Set cursor position (accounting for wrapping)
        if input_inner_width > 0 && self.cursor_pos <= self.input.len() {
            let cursor_row = self.cursor_pos / input_inner_width;
            let cursor_col = self.cursor_pos % input_inner_width;
            let cy = input_area.y + 1 + cursor_row as u16;
            let cx = input_area.x + 1 + cursor_col as u16;
            if cx < input_area.x + input_area.width.saturating_sub(1)
                && cy < input_area.y + input_total_height
            {
                frame.set_cursor(cx, cy);
            }
        }

        // Flash message overlay (e.g. "Copied to clipboard!")
        if let Some((msg, _expiry)) = &self.flash_message {
            let msg_w = (msg.len() as u16).min(chat_area.width.saturating_sub(4));
            let overlay_x = chat_area.x + chat_area.width.saturating_sub(2).saturating_sub(msg_w);
            let overlay_y = chat_area.y + chat_area.height.saturating_sub(2);
            if msg_w > 0 && overlay_x > chat_area.x {
                let overlay_area = ratatui::layout::Rect::new(overlay_x, overlay_y, msg_w + 2, 1);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!(" {}", msg),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD),
                    ))),
                    overlay_area,
                );
            }
        }
    }
}

/// Copy `text` to the system clipboard using whatever tool is available.
fn copy_to_clipboard(text: &str) {
    use std::process::Command;
    // Try wl-clipboard (Wayland), then xclip (X11), then pbcopy (macOS)
    let result = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(text.as_bytes())?;
            child.wait()?;
            Ok(())
        });
    if result.is_err() {
        let _ = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(text.as_bytes())?;
                child.wait()?;
                Ok(())
            });
    }
}

fn tool_input_preview(input: &serde_json::Value) -> String {
    input
        .get("command")
        .or_else(|| input.get("file_path"))
        .or_else(|| input.get("pattern"))
        .and_then(|v| v.as_str())
        .map(|s| if s.len() > 80 { format!("{}…", &s[..80]) } else { s.to_string() })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Inline markdown → styled Spans
// ---------------------------------------------------------------------------

/// Parse inline markdown syntax in `text` and return styled `Span`s.
///
/// Supported:
/// - `**bold**` → `BOLD` modifier
/// - `*italic*` → `ITALIC` modifier
/// - `` `code` `` → `DarkGray` background
/// - `~~strikethrough~~` → `CROSSED_OUT` modifier
/// - `[label](url)` → cyan underline link (label shown, url hidden)
fn parse_inline_markdown(text: &str, base: Style) -> Vec<Span<'static>> {
    if text.is_empty() {
        return vec![];
    }

    enum MdState { Normal, Bold, Italic, Code, Strike, LinkLabel, LinkUrl }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut state = MdState::Normal;
    let mut buf = String::new();
    let mut link_label = String::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            MdState::Normal => {
                match c {
                    '*' if chars.peek() == Some(&'*') => {
                        chars.next();
                        flush_text(&mut spans, &mut buf, base);
                        state = MdState::Bold;
                    }
                    '*' => {
                        flush_text(&mut spans, &mut buf, base);
                        state = MdState::Italic;
                    }
                    '`' => {
                        flush_text(&mut spans, &mut buf, base);
                        state = MdState::Code;
                    }
                    '~' if chars.peek() == Some(&'~') => {
                        chars.next();
                        flush_text(&mut spans, &mut buf, base);
                        state = MdState::Strike;
                    }
                    '[' => {
                        flush_text(&mut spans, &mut buf, base);
                        link_label.clear();
                        state = MdState::LinkLabel;
                    }
                    _ => buf.push(c),
                }
            }
            MdState::Bold => {
                if c == '*' && chars.peek() == Some(&'*') {
                    chars.next();
                    spans.push(Span::styled(
                        std::mem::take(&mut buf),
                        base.add_modifier(Modifier::BOLD),
                    ));
                    state = MdState::Normal;
                } else {
                    buf.push(c);
                }
            }
            MdState::Italic => {
                if c == '*' {
                    spans.push(Span::styled(
                        std::mem::take(&mut buf),
                        base.add_modifier(Modifier::ITALIC),
                    ));
                    state = MdState::Normal;
                } else {
                    buf.push(c);
                }
            }
            MdState::Code => {
                if c == '`' {
                    spans.push(Span::styled(
                        std::mem::take(&mut buf),
                        Style::default().fg(Color::White).bg(Color::DarkGray),
                    ));
                    state = MdState::Normal;
                } else {
                    buf.push(c);
                }
            }
            MdState::Strike => {
                if c == '~' && chars.peek() == Some(&'~') {
                    chars.next();
                    spans.push(Span::styled(
                        std::mem::take(&mut buf),
                        base.add_modifier(Modifier::CROSSED_OUT),
                    ));
                    state = MdState::Normal;
                } else {
                    buf.push(c);
                }
            }
            MdState::LinkLabel => {
                if c == ']' {
                    link_label = std::mem::take(&mut buf);
                    if chars.peek() == Some(&'(') {
                        chars.next();
                        state = MdState::LinkUrl;
                    } else {
                        // Not a real link — emit as plain text
                        spans.push(Span::styled(
                            format!("[{link_label}]"),
                            base,
                        ));
                        state = MdState::Normal;
                    }
                } else {
                    buf.push(c);
                }
            }
            MdState::LinkUrl => {
                if c == ')' {
                    let _url = std::mem::take(&mut buf);
                    spans.push(Span::styled(
                        link_label.clone(),
                        base.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED),
                    ));
                    state = MdState::Normal;
                } else {
                    buf.push(c);
                }
            }
        }
    }

    // Flush remaining buffer
    match state {
        MdState::Normal => flush_text(&mut spans, &mut buf, base),
        MdState::Bold => {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                base.add_modifier(Modifier::BOLD),
            ));
        }
        MdState::Italic => {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                base.add_modifier(Modifier::ITALIC),
            ));
        }
        MdState::Code => {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                Style::default().fg(Color::White).bg(Color::DarkGray),
            ));
        }
        MdState::Strike => {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                base.add_modifier(Modifier::CROSSED_OUT),
            ));
        }
        MdState::LinkLabel => {
            spans.push(Span::styled(
                format!("[{}]", buf),
                base,
            ));
        }
        MdState::LinkUrl => {
            spans.push(Span::styled(
                format!("[{}]({})", link_label, buf),
                base,
            ));
        }
    }

    spans
}

/// Helper: flush plain text buffer as a styled span.
fn flush_text(spans: &mut Vec<Span<'static>>, buf: &mut String, base: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), base));
    }
}

// ---------------------------------------------------------------------------
// Markdown table helpers
// ---------------------------------------------------------------------------

/// Does the line look like a markdown table row (starts with `|`)?
/// Detect markdown heading level (1 = #, 2 = ##, 3+ = ### etc.)
fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    let mut level = 0;
    for c in trimmed.chars() {
        if c == '#' { level += 1; } else { break; }
    }
    if level > 0 && level <= 6 && trimmed.len() > level {
        // Must be followed by a space
        if trimmed.as_bytes().get(level) == Some(&b' ') {
            return Some(level);
        }
    }
    None
}

fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.contains('|')
}

/// Does the line look like a markdown table separator (`|---|---|`)?
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return false;
    }
    // Check that only pipe, dash, colon, and space characters are present
    trimmed[1..].chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Render a markdown table row as styled spans.
/// Cells are separated by vertical bars with a cyan tint.
fn render_table_row(line: &str, base: Style) -> Vec<Span<'static>> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix('|')
        .and_then(|s| s.strip_suffix('|'))
        .unwrap_or(trimmed);
    let cells: Vec<&str> = inner.split('|').map(|c| c.trim()).collect();

    if cells.is_empty() {
        return vec![Span::styled(line.to_string(), base)];
    }

    let mut spans = Vec::new();
    let separator_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
    let cell_style = base;

    // Opening bar
    spans.push(Span::styled("│", separator_style));

    for (idx, cell) in cells.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("│", separator_style));
        }
        // Render inline markdown inside the cell
        spans.extend(parse_inline_markdown(cell, cell_style));
    }

    // Closing bar
    spans.push(Span::styled("│", separator_style));

    spans
}

/// Apply a selection highlight (blue background) to a portion of a Line.
/// `sel_start` and `sel_end` are character offsets within the line.
fn apply_selection_to_line(line: &Line<'_>, sel_start: usize, sel_end: usize) -> Line<'static> {
    let sel_style = Style::default().bg(Color::Rgb(60, 80, 180));
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut offset = 0usize;
    for span in &line.spans {
        let span_len = span.content.chars().count();
        let span_begin = offset;
        let span_end = offset + span_len;

        // Convert span content to an owned String for lifetime freedom
        let text: String = span.content.chars().collect();
        let base_style = span.style;

        if span_end <= sel_start || span_begin >= sel_end {
            // Entirely outside selection
            spans.push(Span::styled(text.clone(), base_style));
        } else if span_begin >= sel_start && span_end <= sel_end {
            // Entirely inside selection
            spans.push(Span::styled(text.clone(), base_style.patch(sel_style)));
        } else {
            // Partially selected — split into up to three segments
            if span_begin < sel_start {
                let prefix: String = text.chars().take(sel_start.saturating_sub(span_begin)).collect();
                if !prefix.is_empty() {
                    spans.push(Span::styled(prefix, base_style));
                }
            }
            let seg_begin = sel_start.max(span_begin);
            let seg_end = sel_end.min(span_end);
            if seg_end > seg_begin {
                let middle: String = text.chars().skip(seg_begin - span_begin).take(seg_end - seg_begin).collect();
                if !middle.is_empty() {
                    spans.push(Span::styled(middle, base_style.patch(sel_style)));
                }
            }
            if span_end > sel_end {
                let suffix: String = text.chars().skip(sel_end.saturating_sub(span_begin)).collect();
                if !suffix.is_empty() {
                    spans.push(Span::styled(suffix, base_style));
                }
            }
        }
        offset = span_end;
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let use_stream = !args.iter().any(|a| a == "--no-stream" || a == "-n");
    let use_think = args.iter().any(|a| a == "--think" || a == "-t");
    let no_cache = args.iter().any(|a| a == "--no-cache");

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        cursor::Hide,
        crossterm::event::EnableMouseCapture,
        EnableBracketedPaste,
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut client = TuiClient::new();
    // Guard with \u{00a0} (non-breaking space) so Wrap { trim: true } doesn't
    // strip the leading spaces that align the omega art.
    // (NBSP is not ASCII whitespace, so trim_start() skips it.)
    client.add_line(Line::from(Span::styled(
        "\u{00a0} .d88888888b. ",
        Style::default().fg(Color::Cyan),
    )));
    client.add_line(Line::from(Span::styled(
        "\u{00a0}d88P\"    \"Y88b",
        Style::default().fg(Color::Cyan),
    )));
    client.add_line(Line::from(Span::styled(
        "\u{00a0}888        888",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    client.add_line(Line::from(Span::styled(
        "\u{00a0}Y88b      d88P",
        Style::default().fg(Color::Cyan),
    )));
    client.add_line(Line::from(Span::styled(
        "\u{00a0} \"88bo  od88\" ",
        Style::default().fg(Color::Cyan),
    )));
    client.add_line(Line::from(Span::styled(
        "\u{00a0}d88888  88888b",
        Style::default().fg(Color::Cyan),
    )));
    client.add_system_msg("Type a message and press Enter. Esc to quit, PgUp/PgDn to scroll.".to_string());
    client.add_separator();

    // Connect to omega-loop
    client.status = "Connecting to omega-loop...".to_string();
    terminal.draw(|frame| client.render(frame))?;

    let daemon = match AgentdClient::connect().await {
        Ok(c) => {
            client.add_system_msg("✓ Connected to omega-loop".to_string());
            client.status = "Ready".to_string();
            c
        }
        Err(e) => {
            client.add_system_msg(format!("❌ Failed to connect: {}", e));
            client.status = "Disconnected".to_string();
            show_until_quit(&mut terminal, &mut client).await?;
            cleanup_terminal(&mut terminal);
            return Ok(());
        }
    };

    client.session_id = format!(
        "picrust-session-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );

    let config = SessionConfig {
        stream: use_stream,
        think: use_think,
        no_cache,
    };

    let res = run_loop(&mut terminal, &mut client, daemon, config).await;
    cleanup_terminal(&mut terminal);

    if let Err(e) = res {
        eprintln!("TUI error: {}", e);
    }
    Ok(())
}

fn cleanup_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let _ = terminal.show_cursor();
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
        DisableBracketedPaste,
    );
}

async fn show_until_quit(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &mut TuiClient,
) -> io::Result<()> {
    while client.running {
        client.tick();
        terminal.draw(|frame| client.render(frame))?;
        if event::poll(Duration::from_millis(100))? {
            let evt = event::read()?;
            match &evt {
                Event::Mouse(mouse) => {
                    let term_area = terminal.size().unwrap_or(ratatui::layout::Rect::new(0,0,80,24));
                    match mouse.kind {
                        MouseEventKind::ScrollDown => {
                            for _ in 0..3 { client.scroll_down_line(); }
                        }
                        MouseEventKind::ScrollUp => {
                            for _ in 0..3 { client.scroll_up_line(); }
                        }
                        MouseEventKind::Down(button) => {
                            if button == crossterm::event::MouseButton::Left {
                                // Check if this is a click on the collapsed thinking line
                                let chat = client.chat_area_from_size(term_area);
                                let inner_y = chat.y + 1;
                                let total_lines = client.chat_lines.len();
                                let visible_lines = chat.height.saturating_sub(2) as usize;
                                let max_scroll = total_lines.saturating_sub(visible_lines);
                                let scroll_start = if client.auto_scroll {
                                    max_scroll
                                } else {
                                    max_scroll.saturating_sub(client.scroll_offset.min(max_scroll))
                                };
                                if mouse.row >= inner_y {
                                    let clicked_line = scroll_start + (mouse.row - inner_y) as usize;
                                    if Some(clicked_line) == client.collapsed_thinking_line {
                                        client.expand_thinking();
                                    } else {
                                        client.start_selection(mouse.column, mouse.row, term_area);
                                    }
                                } else {
                                    client.start_selection(mouse.column, mouse.row, term_area);
                                }
                            }
                        }
                        MouseEventKind::Drag(button) => {
                            if button == crossterm::event::MouseButton::Left {
                                client.update_selection(mouse.column, mouse.row);
                            }
                        }
                        MouseEventKind::Up(button) => {
                            if button == crossterm::event::MouseButton::Left {
                                client.finish_selection(mouse.column, mouse.row, term_area);
                            }
                        }
                        _ => {}
                    }
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match key.code {
                        KeyCode::Esc => {
            if client.processing {
                // Will be handled by the insert-mode Esc below;
                // this just avoids quitting.
            }
        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &mut TuiClient,
    daemon: AgentdClient,
    config: SessionConfig,
) -> Result<()> {
    // Split into independent reader + writer halves so they can be used
    // concurrently without a Mutex (the daemon only sends output events
    // on the connection that issued the "run" request).
    let (mut daemon_reader, mut daemon_writer) = daemon.split();

    // Channel for events from the daemon reader task
    let (event_tx, mut event_rx) = mpsc::channel::<ServerEvent>(256);

    // Spawn a task to read events from the daemon (uses the reader half — no Mutex needed)
    let reader_handle = tokio::spawn(async move {
        loop {
            match daemon_reader.recv_event().await {
                Ok(Some(event)) => {
                    if event_tx.send(event).await.is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = event_tx
                        .send(ServerEvent::Unknown(serde_json::json!({"type": "Disconnected"})))
                        .await;
                    break;
                }
                Err(e) => {
                    tracing::warn!("Daemon read error: {}", e);
                    break;
                }
            }
        }
    });

    let mut first = true;

    while client.running {
        client.tick();
        terminal.draw(|frame| client.render(frame))?;

        // Process events from daemon
        loop {
            match event_rx.try_recv() {
                Ok(ServerEvent::Created { session_name, .. }) => {
                    if first {
                        client.add_system_msg(format!("Session: {}", session_name));
                        first = false;
                    }
                }
                Ok(ServerEvent::Chunk { chunk, .. }) => {
                    match &chunk {
                        OutputChunk::AskUserQuestion { .. } => {
                            client.handle_chunk(&chunk);
                        }
                        OutputChunk::PermissionRequest {
                            tool_name,
                            action,
                            details,
                            ..
                        } => {
                            client.waiting_permission = true;
                            client.permission_tool = tool_name.clone();
                            client.add_system_msg(format!(
                                "🔐 Permission needed: {} — {} {}",
                                tool_name,
                                action,
                                details.as_deref().unwrap_or("")
                            ));
                            client.status =
                                "Permission required (y=allow, n=deny, a=always, d=always deny)"
                                    .to_string();
                        }
                        _ => {
                            let is_done =
                                matches!(&chunk, OutputChunk::Done | OutputChunk::Error(_));
                            client.handle_chunk(&chunk);
                            if is_done {
                                client.processing = false;
                            }
                        }
                    }
                }
                Ok(ServerEvent::SessionList { sessions }) => {
                    client.waiting_session_list = false;
                    if sessions.is_empty() {
                        client.add_system_msg("No saved sessions found.".to_string());
                    } else {
                        client.add_system_msg(
                            format!("Available sessions ({}):", sessions.len())
                        );
                        for s in &sessions {
                            client.add_system_msg(format!("  /resume {}", s));
                        }
                        // Store the list so user can pick
                        client.session_list = sessions;
                    }
                }
                Ok(ServerEvent::SessionResumed { session_id, session_name }) => {
                    client.session_id = session_id.clone();
                    client.chat_lines.clear();
                    client.pending_assistant_idx = None;
                    client.pending_assistant_raw = String::new();
                    client.pending_thinking_idx = None;
                    client.pending_thinking_raw = String::new();
                    client.collapsed_thinking_line = None;
                    client.collapsed_thinking_raw = String::new();
                    client.pending_tool_calls.clear();
                    client.processing = false;
                    first = true;
                    client.add_system_msg(format!("Resumed session: {session_name}"));
                    client.status = "Ready".to_string();
                }
                Ok(ServerEvent::ModelChanged { model }) => {
                    client.add_system_msg(format!("Model changed to: {model}"));
                }
                Ok(ServerEvent::SessionCompacted { .. }) => {
                    client.add_system_msg("Session compacted.".to_string());
                }
                Ok(ServerEvent::SystemMsg(msg)) => {
                    client.add_system_msg(msg);
                }
                Ok(ServerEvent::Unknown(val)) => {
                    if val.get("type").and_then(|v| v.as_str()) == Some("Disconnected") {
                        client.add_system_msg("[omega-loop disconnected]".to_string());
                        client.running = false;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    client.add_system_msg("[omega-loop connection lost]".to_string());
                    client.running = false;
                    break;
                }
            }
        }

        // Handle events (keyboard + mouse) — drain ALL pending events in one batch
        // so pasting large amounts of text processes in one frame (not one render per char).
        let mut events_this_frame = 0u32;
        loop {
            if events_this_frame > 5000 || !event::poll(Duration::ZERO)? {
                break;
            }
            events_this_frame += 1;
            let evt = event::read()?;

            // --- Mouse events: scroll wheel + text selection ---
            if let Event::Mouse(mouse) = &evt {
                let term_area = terminal.size().unwrap_or(ratatui::layout::Rect::new(0,0,80,24));
                match mouse.kind {
                    MouseEventKind::ScrollDown => {
                        for _ in 0..3 { client.scroll_down_line(); }
                    }
                    MouseEventKind::ScrollUp => {
                        for _ in 0..3 { client.scroll_up_line(); }
                    }
                    MouseEventKind::Down(button) => {
                        if button == crossterm::event::MouseButton::Left {
                            // Check if this is a click on the collapsed thinking line
                            let chat = client.chat_area_from_size(term_area);
                            let inner_y = chat.y + 1;
                            let total_lines = client.chat_lines.len();
                            let visible_lines = chat.height.saturating_sub(2) as usize;
                            let max_scroll = total_lines.saturating_sub(visible_lines);
                            let scroll_start = if client.auto_scroll {
                                max_scroll
                            } else {
                                max_scroll.saturating_sub(client.scroll_offset.min(max_scroll))
                            };
                            if mouse.row >= inner_y {
                                let clicked_line = scroll_start + (mouse.row - inner_y) as usize;
                                if Some(clicked_line) == client.collapsed_thinking_line {
                                    client.expand_thinking();
                                } else {
                                    client.start_selection(mouse.column, mouse.row, term_area);
                                }
                            } else {
                                client.start_selection(mouse.column, mouse.row, term_area);
                            }
                        }
                    }
                    MouseEventKind::Drag(button) => {
                        if button == crossterm::event::MouseButton::Left {
                            client.update_selection(mouse.column, mouse.row);
                        }
                    }
                    MouseEventKind::Up(button) => {
                        if button == crossterm::event::MouseButton::Left {
                            client.finish_selection(mouse.column, mouse.row, term_area);
                        }
                    }
                    _ => {}
                }
                continue; // next event in drain
            }

            // --- Permission mode ---
            if client.waiting_permission {
                if let Event::Key(key) = evt {
                    if key.kind == KeyEventKind::Press {
                        let (allowed, _remember) = match key.code {
                            KeyCode::Char('y') => (true, false),
                            KeyCode::Char('n') | KeyCode::Esc => (false, false),
                            KeyCode::Char('a') => (true, true),
                            KeyCode::Char('d') => (false, true),
                            _ => continue,
                        };
                        client.waiting_permission = false;
                        client.status = "Processing...".to_string();
                        client.add_system_msg(format!(
                            "Permission {} for {}",
                            if allowed { "granted" } else { "denied" },
                            client.permission_tool
                        ));
                    }
                }
                continue;
            }

            // --- Question mode ---
            if client.waiting_question {
                if let Event::Key(key) = evt {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                let num = c.to_digit(10).unwrap_or(0) as usize;
                                if let Some(q) = client.questions.first() {
                                    if num >= 1 && num <= q.options.len() {
                                        let mut answers = HashMap::new();
                                        answers.insert(
                                            q.header.clone(),
                                            q.options[num - 1].label.clone(),
                                        );
                                        client.waiting_question = false;
                                        client.status = "Processing...".to_string();

                                        // Send ask-response on the writer half
                                        let _ = daemon_writer
                                            .send_ask_response(
                                                &client.session_id,
                                                &client.question_request_id,
                                                answers,
                                            )
                                            .await;
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                client.waiting_question = false;
                                client.status = "Processing...".to_string();
                            }
                            _ => {}
                        }
                    }
                }
                continue;
            }

            // --- Normal input mode ---
            if let Event::Key(key) = evt {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Enter => {
                            if !client.input.is_empty() && !client.processing && !client.waiting_session_list {
                                let text = std::mem::take(&mut client.input);
                                client.cursor_pos = 0;

                                // --- Slash commands ---
                                if text.starts_with('/') {
                                    let parts: Vec<&str> = text.splitn(2, ' ').collect();
                                    let cmd = parts[0];
                                    let arg = parts.get(1).copied().unwrap_or("");
                                    match cmd {
                                        "/new" | "/new_session" => {
                                            client.session_id = format!(
                                                "picrust-session-{}",
                                                chrono::Local::now().format("%Y%m%d-%H%M%S")
                                            );
                                            client.chat_lines.clear();
                                            client.pending_assistant_idx = None;
                                            client.pending_assistant_raw = String::new();
                                            client.pending_thinking_idx = None;
                                            client.pending_thinking_raw = String::new();
                                            client.collapsed_thinking_line = None;
                                            client.collapsed_thinking_raw = String::new();
                                            client.pending_tool_calls.clear();
                                            client.processing = false;
                                            client.status = "Ready".to_string();
                                            client.add_system_msg(
                                                format!("New session: {}", client.session_id)
                                            );
                                        }
                                        "/model" => {
                                            let model_name = if arg.is_empty() {
                                                "gpt-4o"
                                            } else {
                                                arg
                                            };
                                            let _ = daemon_writer
                                                .send_set_model(&client.session_id, model_name, 16384)
                                                .await;
                                            client.add_system_msg(
                                                format!("Changing model to {model_name}...")
                                            );
                                        }
                                        "/compact" => {
                                            let _ = daemon_writer
                                                .send_compact(&client.session_id)
                                                .await;
                                            client.add_system_msg("Compacting session...".to_string());
                                        }
                                        "/resume" | "/list" => {
                                            if arg.is_empty() {
                                                let _ = daemon_writer
                                                    .send_list_sessions()
                                                    .await;
                                                client.add_system_msg(
                                                    "Requesting session list...".to_string()
                                                );
                                                client.waiting_session_list = true;
                                            } else {
                                                let _ = daemon_writer
                                                    .send_resume_session(arg.trim())
                                                    .await;
                                                client.add_system_msg(
                                                    format!("Resuming session: {}...", arg.trim())
                                                );
                                            }
                                        }
                                        "/quit" => {
                                            client.running = false;
                                        }
                                        "/help" => {
                                            client.add_system_msg(
                                                "Available commands: /new, /resume, /model <name>, /compact, /quit, /help".to_string()
                                            );
                                        }
                                        _ => {
                                            client.add_system_msg(
                                                format!("Unknown command: {cmd}. Type /help for available commands.")
                                            );
                                        }
                                    }
                                } else {
                                    client.add_user_msg(text.clone());
                                    client.add_separator();
                                    client.status = "Processing...".to_string();
                                    client.processing = true;

                                    // Send run on the writer half
                                    let _ = daemon_writer
                                        .send_run(&client.session_id, &text, &config)
                                        .await;
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            if key.modifiers == KeyModifiers::CONTROL && c == 'c' {
                                // Double Ctrl+C within 500ms quits, otherwise interrupt
                                if client.ctrl_c_pressed.map_or(false, |t| t.elapsed() < Duration::from_millis(500)) {
                                    client.running = false;
                                } else {
                                    let _ = daemon_writer.send_interrupt(&client.session_id).await;
                                    client.add_system_msg("Interrupted (press Ctrl+C again within 500ms to quit)".to_string());
                                    client.status = "Ready".to_string();
                                    client.processing = false;
                                    client.ctrl_c_pressed = Some(Instant::now());
                                }
                            } else if key.modifiers == KeyModifiers::CONTROL && c == 'd' {
                                client.running = false;
                            } else if key.modifiers == KeyModifiers::CONTROL && c == 'l' {
                                client.clear();
                            } else {
                                client.command_selection = None;
                                let byte_idx = client.cursor_byte();
                                client.input.insert(byte_idx, c);
                                client.cursor_pos += 1;
                            }
                        }
                        // --- Command preview navigation ---
                        KeyCode::Tab => {
                            if client.input.starts_with('/') && !client.processing {
                                let matches = matching_commands(&client.input);
                                if let Some((_, cmd)) = matches.first() {
                                    // Replace input with the full command name + space
                                    client.input = format!("{} ", cmd.name);
                                    client.cursor_pos = client.char_count();
                                    client.command_selection = None;
                                }
                            }
                        }
                        KeyCode::Up if client.input.starts_with('/') && !client.processing => {
                            let matches = matching_commands(&client.input);
                            if !matches.is_empty() {
                                let cur = client.command_selection.unwrap_or(0);
                                let next = if cur == 0 { matches.len() - 1 } else { cur - 1 };
                                client.command_selection = Some(next);
                                // Update input with selected command
                                if let Some((_, cmd)) = matches.get(next) {
                                    client.input = cmd.name.to_string();
                                    client.cursor_pos = client.char_count();
                                }
                            }
                        }
                        KeyCode::Down if client.input.starts_with('/') && !client.processing => {
                            let matches = matching_commands(&client.input);
                            if !matches.is_empty() {
                                let cur = client.command_selection.unwrap_or(0);
                                let next = if cur >= matches.len() - 1 { 0 } else { cur + 1 };
                                client.command_selection = Some(next);
                                if let Some((_, cmd)) = matches.get(next) {
                                    client.input = cmd.name.to_string();
                                    client.cursor_pos = client.char_count();
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            if client.cursor_pos > 0 {
                                let byte_idx = client.input
                                    .char_indices()
                                    .nth(client.cursor_pos - 1)
                                    .map(|(i, _)| i)
                                    .unwrap_or(0);
                                client.input.remove(byte_idx);
                                client.cursor_pos -= 1;
                                client.command_selection = None;
                            }
                        }
                        KeyCode::Delete => {
                            if client.cursor_pos < client.char_count() {
                                let byte_idx = client.input
                                    .char_indices()
                                    .nth(client.cursor_pos)
                                    .map(|(i, _)| i)
                                    .unwrap_or(client.input.len());
                                if byte_idx < client.input.len() {
                                    client.input.remove(byte_idx);
                                }
                                client.command_selection = None;
                            }
                        }
                        KeyCode::Left => {
                            if client.cursor_pos > 0 {
                                client.cursor_pos -= 1;
                            }
                        }
                        KeyCode::Right => {
                            if client.cursor_pos < client.char_count() {
                                client.cursor_pos += 1;
                            }
                        }
                        // --- Scrolling the chat area ---
                        KeyCode::PageUp => {
                            let h = terminal.size().map(|s| s.height).unwrap_or(24) as usize;
                            let visible = h.saturating_sub(1 + 3 + 2);
                            client.scroll_up_page(visible);
                        }
                        KeyCode::PageDown => {
                            let h = terminal.size().map(|s| s.height).unwrap_or(24) as usize;
                            let visible = h.saturating_sub(1 + 3 + 2);
                            client.scroll_down_page(visible);
                        }
                        KeyCode::Home if key.modifiers == KeyModifiers::CONTROL => {
                            client.auto_scroll = false;
                            let total = client.chat_lines.len();
                            let h = terminal.size().map(|s| s.height).unwrap_or(24) as usize;
                            let visible = h.saturating_sub(1 + 3 + 2);
                            client.scroll_offset = total.saturating_sub(visible);
                        }
                        KeyCode::End if key.modifiers == KeyModifiers::CONTROL => {
                            client.scroll_to_bottom();
                        }
                        KeyCode::Up if key.modifiers == KeyModifiers::CONTROL => {
                            client.scroll_up_line();
                        }
                        KeyCode::Down if key.modifiers == KeyModifiers::CONTROL => {
                            client.scroll_down_line();
                        }
                        // --- Input editing ---
                        KeyCode::Home => client.cursor_pos = 0,
                        KeyCode::End => client.cursor_pos = client.char_count(),
                        KeyCode::Esc => {
                            if client.processing {
                                let _ = daemon_writer.send_interrupt(&client.session_id).await;
                                client.add_system_msg("Interrupted".to_string());
                                client.status = "Ready".to_string();
                                client.processing = false;
                            }
                        }
                        _ => {}
                    }
                }
            }

            // --- Paste event: insert all text verbatim (no Enter triggers) ---
            if let Event::Paste(pasted) = &evt {
                if !client.waiting_permission && !client.waiting_question {
                    let mut byte_idx = client.cursor_byte();
                    for c in pasted.chars() {
                        client.input.insert(byte_idx, c);
                        byte_idx += c.len_utf8();
                    }
                    client.cursor_pos += pasted.chars().count();
                }
                continue;
            }
        }

        tokio::task::yield_now().await;
    }

    // Cleanup
    reader_handle.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matching_commands_empty_input() {
        let matches = matching_commands("");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_matching_commands_non_slash() {
        let matches = matching_commands("hello");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_matching_commands_just_slash() {
        let matches = matching_commands("/");
        // All commands start with '/', so all should match
        assert_eq!(matches.len(), COMMANDS.len());
    }

    #[test]
    fn test_matching_commands_new_prefix() {
        let matches = matching_commands("/n");
        // Should match /new only
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1.name, "/new");
    }

    #[test]
    fn test_matching_commands_resume_prefix() {
        let matches = matching_commands("/res");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1.name, "/resume");
    }

    #[test]
    fn test_matching_commands_full_command() {
        let matches = matching_commands("/compact");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1.name, "/compact");
    }

    #[test]
    fn test_matching_commands_case_insensitive() {
        let matches = matching_commands("/New");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1.name, "/new");
    }

    #[test]
    fn test_matching_commands_unknown_prefix() {
        let matches = matching_commands("/xyz");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_format_command_no_args() {
        let cmd = CommandDef { name: "/new", args: "", desc: "test" };
        assert_eq!(format_command(&cmd), "/new");
    }

    #[test]
    fn test_format_command_with_args() {
        let cmd = CommandDef { name: "/model", args: "<name>", desc: "test" };
        assert_eq!(format_command(&cmd), "/model <name>");
    }

    #[test]
    fn test_command_defs_are_consistent() {
        // All commands should start with /
        for cmd in COMMANDS {
            assert!(cmd.name.starts_with('/'), "Command '{}' must start with /", cmd.name);
            assert!(!cmd.desc.is_empty(), "Command '{}' must have a description", cmd.name);
        }
    }

    #[test]
    fn test_no_duplicate_commands() {
        let mut names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), COMMANDS.len(), "Duplicate command names found");
    }
}
