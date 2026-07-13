//! TUI Renderer - Subscribes to an agent and renders output in the TUI
//!
//! The `TuiRenderer` is an alternative to `ConsoleRenderer` that uses
//! Ratatui to render a rich terminal UI with chat display, input bar,
//! and status indicators.

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::core::{InputMessage, OutputChunk};
use crate::runtime::AgentHandle;

use super::app::{Action, AppMode, TuiApp};

/// TUI renderer that subscribes to an agent and renders output
///
/// # Example
///
/// ```ignore
/// let handle = runtime.spawn(session, agent_fn).await;
/// let mut renderer = TuiRenderer::new(handle);
/// renderer.run().await?;
/// ```
pub struct TuiRenderer {
    /// The TUI application state
    app: TuiApp,

    /// Tick interval for UI updates (in milliseconds)
    tick_rate: Duration,

    /// Whether to show thinking blocks
    show_thinking: bool,

    /// Whether to show tool execution details
    show_tools: bool,
}

impl TuiRenderer {
    /// Create a new TUI renderer for an agent
    pub fn new(handle: AgentHandle) -> Self {
        let app = TuiApp::with_handle(handle);

        Self {
            app,
            tick_rate: Duration::from_millis(100),
            show_thinking: true,
            show_tools: true,
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

    /// Set the tick rate for UI updates
    pub fn tick_rate(mut self, duration: Duration) -> Self {
        self.tick_rate = duration;
        self
    }

    /// Run the TUI renderer
    ///
    /// This enters the alternate screen and starts the main event loop.
    pub async fn run(&mut self) -> io::Result<()> {
        // Setup terminal
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Clear screen
        terminal.clear()?;

        // Show welcome banner
        self.app.add_system_message("=== Picrust TUI ===");
        self.app.add_system_message("Type a message and press Enter. Press Esc to quit, Ctrl+C to interrupt.");
        self.app.add_separator();

        // Run the event loop
        let res = self.run_event_loop(&mut terminal).await;

        // Restore terminal
        let _ = terminal.show_cursor();
        terminal::disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

        if let Err(e) = res {
            eprintln!("TUI error: {}", e);
        }

        Ok(())
    }

    /// The main event loop - handles both agent output and keyboard input
    async fn run_event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> io::Result<()> {
        // Check if we have a handle
        let has_handle = self.app.handle().is_some();

        // Subscribe to agent output if we have a handle
        let mut output_rx = self.app.handle().and_then(|h| {
            let rx = h.subscribe();
            Some(rx)
        });

        // Main loop
        loop {
            // Render the UI
            terminal.draw(|frame| {
                self.app.render(frame);
            })?;

            // If no handle, just wait for keyboard events
            if !has_handle {
                if event::poll(self.tick_rate)? {
                    let event = event::read()?;
                    if let Some(action) = self.app.handle_event(&event)? {
                        match action {
                            Action::Quit => {
                                self.app.stop();
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                continue;
            }

            // Process output chunks from the agent
            {
                if let Some(ref mut rx) = output_rx {
                    // Drain all available chunks
                    loop {
                        match rx.try_recv() {
                            Ok(chunk) => {
                                let is_done = matches!(
                                    chunk,
                                    OutputChunk::Done | OutputChunk::Error(_)
                                );

                                self.app.process_chunk(&chunk);

                                if is_done {
                                    self.app.set_mode(AppMode::Idle);
                                    self.app.set_status("Ready");
                                }

                                // Check if we need user input for permission or question
                                if matches!(chunk, OutputChunk::PermissionRequest { .. }) {
                                    // Will be handled below
                                } else if matches!(chunk, OutputChunk::AskUserQuestion { .. }) {
                                    // Will be handled below
                                }
                            }
                            Err(_) => break, // No more chunks
                        }
                    }
                }
            }

            // If permission or question was requested, handle it
            if self.app.mode() == AppMode::PermissionRequest {
                self.handle_permission_prompt(terminal).await?;
            } else if self.app.mode() == AppMode::Question {
                self.handle_question_prompt(terminal).await?;
            }

            // Check for keyboard events
            if event::poll(self.tick_rate)? {
                let event = event::read()?;
                if let Some(action) = self.app.handle_event(&event)? {
                    match action {
                        Action::SendMessage(text) => {
                            // Send message to agent
                            self.app.add_message("user", &text);
                            self.app.add_separator();
                            self.app.set_mode(AppMode::Processing);
                            self.app.set_status("Processing...");

                            if let Some(ref h) = self.app.handle().cloned() {
                                let _ = h.send_input(&text).await;
                            }
                        }
                        Action::Interrupt => {
                            if let Some(ref h) = self.app.handle() {
                                let _ = h.send(InputMessage::Interrupt).await;
                            }
                            self.app.add_system_message("Interrupted");
                            self.app.set_mode(AppMode::Idle);
                            self.app.set_status("Ready");
                        }
                        Action::Quit => {
                            self.app.stop();
                            if let Some(ref h) = self.app.handle() {
                                let _ = h.shutdown().await;
                            }
                            break;
                        }
                        Action::Clear => {
                            self.app.chat_mut().clear();
                        }
                        _ => {} // Permission and question actions handled separately
                    }
                }
            }

            // Allow other tasks to run
            tokio::task::yield_now().await;
        }

        Ok(())
    }

    /// Handle permission prompt - blocks until user responds
    async fn handle_permission_prompt(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> io::Result<()> {
        self.app.set_mode(AppMode::PermissionRequest);

        loop {
            terminal.draw(|frame| {
                self.app.render(frame);
            })?;

            if event::poll(Duration::from_millis(50))? {
                let event = event::read()?;
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        let (allowed, remember) = match key.code {
                            KeyCode::Char('y') => (true, false),
                            KeyCode::Char('n') | KeyCode::Esc => (false, false),
                            KeyCode::Char('a') => (true, true),
                            KeyCode::Char('d') => (false, true),
                            _ => continue,
                        };

                        // Send permission response to agent
                        if let Some(ref h) = self.app.handle() {
                            let tool_name = self.app.pending_permission_tool()
                                .unwrap_or("tool")
                                .to_string();
                            let _ = h
                                .send_permission_response(&tool_name, allowed, remember)
                                .await;
                        }

                        self.app.set_mode(AppMode::Idle);
                        self.app.set_status("Ready");
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Handle question prompt - blocks until user answers
    async fn handle_question_prompt(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> io::Result<()> {
        self.app.set_mode(AppMode::Question);

        // We need to know how many options there are. For now, we look at the last
        // system message which lists the options.
        // Actually, the questions are stored in the chat history as system messages.
        // We can't easily extract them. For now, we just wait for a number input.
        self.app.set_status("Press number to select an option, Esc to skip");

        let mut answer: Option<String> = None;

        loop {
            terminal.draw(|frame| {
                self.app.render(frame);
            })?;

            if event::poll(Duration::from_millis(50))? {
                let event = event::read()?;
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                let num = c.to_digit(10).unwrap_or(0) as usize;
                                answer = Some(num.to_string());
                                break;
                            }
                            KeyCode::Esc => {
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // Send the response back to the agent
        if let Some(ref h) = self.app.handle() {
            if let Some(request_id) = self.app.pending_question_id() {
                let mut answers = HashMap::new();
                if let Some(ans) = answer {
                    answers.insert("response".to_string(), ans);
                }
                let msg = InputMessage::UserQuestionResponse {
                    request_id: request_id.to_string(),
                    answers,
                };
                let _ = h.send(msg).await;
            }
        }

        self.app.set_mode(AppMode::Idle);
        self.app.set_status("Ready");
        Ok(())
    }
}

impl Drop for TuiRenderer {
    fn drop(&mut self) {
        // Ensure terminal is restored
        let _ = terminal::disable_raw_mode();
    }
}
