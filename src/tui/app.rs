//! Main TUI application - state machine and event loop
//!
//! This module provides the `TuiApp` struct which manages the terminal UI state,
//! handles keyboard events, and coordinates rendering via Ratatui.

use std::io;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::core::OutputChunk;
use crate::runtime::AgentHandle;

use super::widgets::{ChatWidget, InputWidget, StatusWidget};

/// Main TUI application state
pub struct TuiApp {
    /// The agent handle for communication
    handle: Option<AgentHandle>,

    /// Chat history as display-ready lines
    chat: ChatWidget,

    /// Input bar state
    input: InputWidget,

    /// Status bar state
    status: StatusWidget,

    /// Whether the application is running
    running: bool,

    /// Whether to show thinking blocks
    show_thinking: bool,

    /// Whether to show tool execution details
    show_tools: bool,

    /// Input mode - whether we're waiting for input or processing
    mode: AppMode,

    /// The tool name from the last permission request (for sending response)
    pending_permission_tool: Option<String>,

    /// The request_id from the last AskUserQuestion (for sending response)
    pending_question_id: Option<String>,

    /// Time of last Ctrl+C press (for double-press-to-quit detection)
    last_interrupt_time: Option<Instant>,
}

/// Application mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Idle - waiting for user input
    Idle,
    /// Processing - agent is responding
    Processing,
    /// Permission request - waiting for user decision
    PermissionRequest,
    /// Question - waiting for user answer
    Question,
}

impl TuiApp {
    /// Create a new TUI application
    pub fn new() -> Self {
        Self {
            handle: None,
            chat: ChatWidget::new(),
            input: InputWidget::new(),
            status: StatusWidget::new(),
            running: true,
            show_thinking: true,
            show_tools: true,
            mode: AppMode::Idle,
            pending_permission_tool: None,
            pending_question_id: None,
            last_interrupt_time: None,
        }
    }

    /// Create a TUI app with an agent handle
    pub fn with_handle(handle: AgentHandle) -> Self {
        Self {
            handle: Some(handle),
            chat: ChatWidget::new(),
            input: InputWidget::new(),
            status: StatusWidget::new(),
            running: true,
            show_thinking: true,
            show_tools: true,
            mode: AppMode::Idle,
            pending_permission_tool: None,
            pending_question_id: None,
            last_interrupt_time: None,
        }
    }

    /// Set whether to show thinking blocks
    pub fn show_thinking(mut self, show: bool) -> Self {
        self.show_thinking = show;
        self
    }

    /// Set whether to show tool execution details
    pub fn show_tools(mut self, show: bool) -> Self {
        self.show_tools = show;
        self
    }

    /// Get the agent handle
    pub fn handle(&self) -> Option<&AgentHandle> {
        self.handle.as_ref()
    }

    /// Get mutable reference to the agent handle
    pub fn handle_mut(&mut self) -> Option<&mut AgentHandle> {
        self.handle.as_mut()
    }

    /// Add a message to the chat display
    pub fn add_message(&mut self, role: &str, content: &str) {
        self.chat.add_message(role, content);
    }

    /// Add a system message
    pub fn add_system_message(&mut self, content: &str) {
        self.chat.add_system_message(content);
    }

    /// Add a tool execution entry
    pub fn add_tool_call(&mut self, id: &str, tool_name: &str, args: &str) {
        self.chat.add_tool_call(id, tool_name, args);
    }

    /// Update a tool call with result (matched by ID)
    pub fn update_tool_result(&mut self, id: &str, success: bool, message: &str) {
        self.chat.update_tool_result(id, success, message);
    }

    /// Add a thinking block
    pub fn add_thinking(&mut self, content: &str) {
        if self.show_thinking {
            self.chat.add_thinking(content);
        }
    }

    /// Add a separator to the chat
    pub fn add_separator(&mut self) {
        self.chat.add_separator();
    }

    /// Get a mutable reference to the chat widget
    pub fn chat_mut(&mut self) -> &mut ChatWidget {
        &mut self.chat
    }

    /// Set the status message
    pub fn set_status(&mut self, status: &str) {
        self.status.set_status(status);
    }

    /// Set the application mode
    pub fn set_mode(&mut self, mode: AppMode) {
        self.mode = mode;
    }

    /// Get the current mode
    pub fn mode(&self) -> AppMode {
        self.mode
    }

    /// Get the tool name from the last permission request
    pub fn pending_permission_tool(&self) -> Option<&str> {
        self.pending_permission_tool.as_deref()
    }

    /// Get the request ID from the last AskUserQuestion
    pub fn pending_question_id(&self) -> Option<&str> {
        self.pending_question_id.as_deref()
    }

    /// Get the last Ctrl+C press time (for double-press detection)
    pub fn last_interrupt_time(&self) -> Option<Instant> {
        self.last_interrupt_time
    }

    /// Set the last Ctrl+C press time
    pub fn set_last_interrupt_time(&mut self, time: Option<Instant>) {
        self.last_interrupt_time = time;
    }

    /// Get the input text
    pub fn input_text(&self) -> &str {
        self.input.text()
    }

    /// Clear the input
    pub fn clear_input(&mut self) {
        self.input.clear();
    }

    /// Check if the app should keep running
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Signal the app to stop
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Render the UI into the given frame
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.size();

        // Create layout: main chat area, optional status bar, input bar
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),    // Main content (chat)
                Constraint::Length(1), // Status bar
                Constraint::Length(3), // Input bar
            ])
            .split(area);

        // Render chat widget
        self.chat.render(frame, chunks[0]);

        // Render status bar
        self.status.render(frame, chunks[1]);

        // Render input bar
        self.input.render(frame, chunks[2]);
    }

    /// Handle a terminal event
    pub fn handle_event(&mut self, event: &Event) -> io::Result<Option<Action>> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // Global: Ctrl+C double-press to quit (works in all modes)
                if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                    let now = Instant::now();
                    let is_double = self.last_interrupt_time
                        .map_or(false, |t| now.duration_since(t).as_secs_f64() < 1.0);
                    if is_double {
                        return Ok(Some(Action::Quit));
                    }
                    self.last_interrupt_time = Some(now);
                    self.status.set_status("Press Ctrl+C again to exit");
                    return Ok(Some(Action::Interrupt));
                }

                match self.mode {
                    AppMode::PermissionRequest => {
                        return self.handle_permission_key(key);
                    }
                    AppMode::Question => {
                        return self.handle_question_key(key);
                    }
                    _ => {
                        return self.handle_input_key(key);
                    }
                }
            }
            Event::Resize(cols, rows) => {
                self.status.set_status(&format!("Terminal: {}x{}", cols, rows));
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle keyboard input in normal mode
    fn handle_input_key(&mut self, key: &crossterm::event::KeyEvent) -> io::Result<Option<Action>> {
        match key.code {
            KeyCode::Enter => {
                let text = self.input.text().to_string();
                if !text.is_empty() {
                    if text.trim() == "/quit" {
                        self.clear_input();
                        return Ok(Some(Action::Quit));
                    }
                    self.clear_input();
                    return Ok(Some(Action::SendMessage(text)));
                }
            }
            KeyCode::Char(c) => {
                // Ctrl+C is handled globally in handle_event
                if key.modifiers == KeyModifiers::CONTROL && c == 'd' {
                    return Ok(Some(Action::Quit));
                }
                if key.modifiers == KeyModifiers::CONTROL && c == 'l' {
                    return Ok(Some(Action::Clear));
                }
                self.input.insert_char(c);
            }
            KeyCode::Backspace => {
                self.input.delete_char();
            }
            KeyCode::Delete => {
                self.input.delete_forward();
            }
            KeyCode::Left => {
                self.input.move_cursor_left();
            }
            KeyCode::Right => {
                self.input.move_cursor_right();
            }
            KeyCode::Home => {
                self.input.move_cursor_home();
            }
            KeyCode::End => {
                self.input.move_cursor_end();
            }
            KeyCode::Esc => {
                self.last_interrupt_time = None;
                self.status.set_status("Ready");
                return Ok(Some(Action::Interrupt));
            }
            _ => {}
        }
        Ok(None)
    }

    /// Handle keyboard input during permission requests
    fn handle_permission_key(&mut self, key: &crossterm::event::KeyEvent) -> io::Result<Option<Action>> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                Ok(Some(Action::PermissionResponse(true, false)))
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                Ok(Some(Action::PermissionResponse(false, false)))
            }
            KeyCode::Char('a') => {
                Ok(Some(Action::PermissionResponse(true, true)))
            }
            KeyCode::Char('d') => {
                Ok(Some(Action::PermissionResponse(false, true)))
            }
            _ => Ok(None),
        }
    }

    /// Handle keyboard input during questions
    fn handle_question_key(&mut self, key: &crossterm::event::KeyEvent) -> io::Result<Option<Action>> {
        match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let num = c.to_digit(10).unwrap_or(0) as usize;
                Ok(Some(Action::QuestionAnswer(num)))
            }
            KeyCode::Esc => {
                Ok(Some(Action::QuestionAnswer(0)))
            }
            _ => Ok(None),
        }
    }

    /// Process an output chunk from the agent
    pub fn process_chunk(&mut self, chunk: &OutputChunk) {
        match chunk {
            OutputChunk::TextDelta(text) => {
                self.chat.append_to_last_text(text);
                self.mode = AppMode::Processing;
            }
            OutputChunk::TextComplete(_) => {
                // Text block complete
            }
            OutputChunk::ThinkingDelta(text) => {
                if self.show_thinking {
                    self.chat.append_to_last_thinking(text);
                }
            }
            OutputChunk::ThinkingComplete(_) => {
                // Thinking block complete
            }
            OutputChunk::ToolStart { id, name, input, .. } => {
                if self.show_tools {
                    let args = format_tool_args(name, input);
                    self.chat.add_tool_call(id, name, &args);
                }
            }
            OutputChunk::ToolProgress { .. } => {
                // Could show progress
            }
            OutputChunk::ToolEnd { id, result, .. } => {
                if self.show_tools {
                    let is_success = !result.is_error;
                    let msg = match &result.content {
                        crate::tools::ToolResultData::Text(t) => t.clone(),
                        _ => String::new(),
                    };
                    self.chat.update_tool_result(id, is_success, &msg);
                }
            }
            OutputChunk::PermissionRequest {
                tool_name,
                action,
                details,
                ..
            } => {
                self.pending_permission_tool = Some(tool_name.clone());
                self.add_system_message(&format!(
                    "🔐 Permission needed: {} - {} {}",
                    tool_name,
                    action,
                    details.as_deref().unwrap_or("")
                ));
                self.mode = AppMode::PermissionRequest;
                self.status
                    .set_status("Permission required (y=allow, n=deny, a=always, d=always deny)");
            }
            OutputChunk::AskUserQuestion {
                request_id,
                questions,
            } => {
                self.pending_question_id = Some(request_id.clone());
                self.add_system_message("❓ Agent has a question:");
                for q in questions {
                    self.add_system_message(&format!("  [{}] {}", q.header, q.question));
                    for (i, opt) in q.options.iter().enumerate() {
                        self.add_system_message(&format!(
                            "    {}. {} - {}",
                            i + 1,
                            opt.label,
                            opt.description
                        ));
                    }
                }
                self.mode = AppMode::Question;
                self.status.set_status("Select an option (number)");
            }
            OutputChunk::Status(status) => {
                self.add_system_message(status);
                self.status.set_status(status);
            }
            OutputChunk::Error(e) => {
                self.add_system_message(&format!("❌ Error: {}", e));
                self.mode = AppMode::Idle;
            }
            OutputChunk::Done => {
                self.mode = AppMode::Idle;
                self.status.set_status("Ready");
            }
            OutputChunk::StateChange(state) => {
                self.status
                    .set_status(&format!("State: {:?}", state));
            }
            OutputChunk::SubAgentSpawned {
                session_id,
                agent_type,
            } => {
                self.add_system_message(&format!(
                    "🔄 Spawned subagent: {} ({})",
                    agent_type, session_id
                ));
            }
            OutputChunk::SubAgentComplete { session_id, result } => {
                self.add_system_message(&format!(
                    "✅ Subagent {} completed: {:?}",
                    session_id, result
                ));
            }
            _ => {}
        }
    }
}

impl Default for TuiApp {
    fn default() -> Self {
        Self::new()
    }
}

/// Actions that can be triggered by user input
#[derive(Debug, Clone)]
pub enum Action {
    /// Send a message to the agent
    SendMessage(String),
    /// Interrupt the agent
    Interrupt,
    /// Quit the application
    Quit,
    /// Clear the screen
    Clear,
    /// Permission response (allowed, remember)
    PermissionResponse(bool, bool),
    /// Question answer (option index)
    QuestionAnswer(usize),
}

/// Format key arguments for a tool into a one-liner string.
fn format_tool_args(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Read" | "Write" | "Edit" => {
            if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
                path.to_string()
            } else if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                path.to_string()
            } else {
                String::new()
            }
        }
        "Bash" => {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                let truncated = if cmd.len() > 80 {
                    format!("{}…", &cmd[..80])
                } else {
                    cmd.to_string()
                };
                format!("$ {}", truncated)
            } else {
                String::new()
            }
        }
        "Grep" => {
            if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
                format!("\"{}\"", pattern)
            } else {
                String::new()
            }
        }
        "Glob" => {
            if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
                pattern.to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}
