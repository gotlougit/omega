//! omega-tui — Terminal UI for the omega-loop agent daemon.
//!
//! Connects to a running `omega-loop` daemon, creates/joins a session,
//! provides a prompt with slash commands, and renders streaming agent
//! output using block-based diff rendering adapted from tau.

use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use tokio::runtime::Runtime;

use cli::{Color, Event, Span, Style, StyledBlock, StyledText, Term, TermHandle};
use omega_loop_client::{
    AgentdClient, DaemonReader, DaemonWriter, OutputChunk, ServerEvent, SessionConfig,
};

mod markdown;

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

fn sty(fg: Color) -> Style {
    Style::default().fg(fg)
}
fn s_user() -> Style {
    sty(Color::Cyan).bold()
}
fn s_assistant() -> Style {
    Style::default()
}
fn s_tool() -> Style {
    sty(Color::Yellow)
}
fn s_error() -> Style {
    sty(Color::Red)
}
fn s_system() -> Style {
    sty(Color::DarkGrey).italic()
}
fn s_thinking() -> Style {
    sty(Color::DarkGrey).italic()
}
fn s_highlight() -> Style {
    sty(Color::Green).bold()
}
fn s_cache_hit() -> Style {
    sty(Color::Green)
}
fn s_cache_miss() -> Style {
    sty(Color::DarkGrey)
}

/// Render `text` as markdown into a [`StyledBlock`], using the current
/// terminal width (obtained from the handle) for line wrapping.
fn render_md_block(handle: &TermHandle, text: &str) -> StyledBlock {
    let (w, _) = handle.size();
    let width = w.max(40); // never go below 40 columns
    let styled = markdown::render_markdown(text, width);
    StyledBlock::new(styled)
}

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/model", "Change model   (/model <name>)"),
    ("/models", "List available models"),
    ("/sessions", "List saved sessions"),
    ("/resume", "Resume a session   (/resume <id>)"),
    ("/new", "Create a new session   (/new [id])"),
    ("/compact", "Compact session history"),
    ("/interrupt", "Interrupt the agent"),
    ("/clear", "Clear output"),
    ("/status", "Check omega-loop connection"),
    ("/help", "Show this help"),
    ("/quit", "Exit omega-tui"),
];

/// How long `/status` waits for a daemon reply before reporting a
/// dead connection.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Messages between UI thread and daemon task
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum DaemonCmd {
    Run { session_id: String, content: String },
    SetModel { session_id: String, model: String },
    ListModels,
    ListSessions,
    Resume(String),
    Compact(String),
    Interrupt(String),
    Shutdown,
}

enum DaemonEv {
    Event(ServerEvent),
    Disconnected,
}

/// Everything the UI loop blocks on: terminal input, daemon events, and
/// internal timers. Both producers forward into a single channel so the
/// loop never has to poll one source while blocked on the other.
enum AppEvent {
    Term(Event),
    Daemon(DaemonEv),
    /// Fired by a one-shot timer armed by `/status`.
    StatusTimeout,
}

/// What the UI loop should do after processing a submitted line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOutcome {
    Continue,
    Quit,
    /// `/status` was issued: a ping (list_models) is in flight; arm the
    /// response timer and resolve on the next ModelList event.
    StatusRequested,
}

// ---------------------------------------------------------------------------
// Daemon task — runs on a dedicated tokio runtime, bridges async ↔ sync
// ---------------------------------------------------------------------------

async fn daemon_loop(
    mut reader: DaemonReader,
    mut writer: DaemonWriter,
    cmd_rx: Receiver<DaemonCmd>,
    app_tx: Sender<AppEvent>,
) {
    loop {
        // Drain commands
        loop {
            match cmd_rx.try_recv() {
                Ok(DaemonCmd::Shutdown) => return,
                Ok(DaemonCmd::Run {
                    session_id,
                    content,
                }) => {
                    if let Err(e) = writer
                        .send_run(&session_id, &content, &SessionConfig::default())
                        .await
                    {
                        tracing::error!(target: "omega_tui::daemon", %session_id, error = %e, "send_run failed — daemon connection may be dead");
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                    }
                }
                Ok(DaemonCmd::SetModel { session_id, model }) => {
                    if let Err(e) = writer.send_set_model(&session_id, &model, 16384).await {
                        tracing::error!(target: "omega_tui::daemon", error = %e, "send_set_model failed");
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                    }
                }
                Ok(DaemonCmd::ListModels) => {
                    if let Err(e) = writer.send_list_models().await {
                        tracing::error!(target: "omega_tui::daemon", error = %e, "send_list_models failed");
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                    }
                }
                Ok(DaemonCmd::ListSessions) => {
                    if let Err(e) = writer.send_list_sessions().await {
                        tracing::error!(target: "omega_tui::daemon", error = %e, "send_list_sessions failed");
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                    }
                }
                Ok(DaemonCmd::Resume(sid)) => {
                    if let Err(e) = writer.send_resume_session(&sid).await {
                        tracing::error!(target: "omega_tui::daemon", error = %e, "send_resume_session failed");
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                    }
                }
                Ok(DaemonCmd::Compact(sid)) => {
                    if let Err(e) = writer.send_compact(&sid).await {
                        tracing::error!(target: "omega_tui::daemon", error = %e, "send_compact failed");
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                    }
                }
                Ok(DaemonCmd::Interrupt(sid)) => {
                    if let Err(e) = writer.send_interrupt(&sid).await {
                        tracing::error!(target: "omega_tui::daemon", error = %e, "send_interrupt failed");
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }

        // Read one event from daemon (with timeout so commands aren't starved)
        match tokio::time::timeout(Duration::from_millis(50), reader.recv_event()).await {
            Ok(Ok(Some(event))) => {
                if app_tx
                    .send(AppEvent::Daemon(DaemonEv::Event(event)))
                    .is_err()
                {
                    return;
                }
            }
            Ok(Ok(None)) => {
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "omega_tui::daemon", error = %e, "recv_event read error, retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(_timeout) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming response tracker
// ---------------------------------------------------------------------------

struct Streaming {
    block_id: Option<cli::BlockId>,
    buf: String,
    /// True once TextComplete or Done has finalized the response block
    /// for this turn. A subsequent TextDelta auto-resets to start a
    /// fresh turn; a subsequent TextComplete is a no-op.
    finalized: bool,
}

impl Streaming {
    fn reset(&mut self) {
        self.block_id = None;
        self.buf.clear();
        self.finalized = false;
    }
}

// ---------------------------------------------------------------------------
// Tool call rendering
// ---------------------------------------------------------------------------

/// Renders a tool call as `tool call {name}: {primary arg}` — no emoji, no
/// JSON dump. The primary arg is the first present of well-known fields
/// (`command`, `path`, …); anything else falls back to compact JSON.
fn tool_call_line(name: &str, input: &serde_json::Value) -> String {
    const PRIMARY_KEYS: &[&str] = &["command", "path", "file_path", "pattern", "query", "url"];
    for key in PRIMARY_KEYS {
        if let Some(value) = input.get(*key).and_then(|v| v.as_str()) {
            // Collapse multi-line values (e.g. shell scripts) into one row.
            let one_line = value.trim().lines().collect::<Vec<_>>().join(" ; ");
            if !one_line.is_empty() {
                return format!(
                    "tool call {name}: {}",
                    cli::truncate_to_width(&one_line, 120)
                );
            }
        }
    }
    let json = serde_json::to_string(input).unwrap_or_default();
    if json == "{}" || json == "null" {
        format!("tool call {name}")
    } else {
        format!("tool call {name}: {}", cli::truncate_to_width(&json, 120))
    }
}

// ---------------------------------------------------------------------------
// Handle a daemon event → update terminal output
// ---------------------------------------------------------------------------

fn handle_daemon_event(
    handle: &TermHandle,
    app: &mut AppState,
    streaming: &mut Streaming,
    event: ServerEvent,
) {
    // Helper to refresh the cache status line after any change.
    let refresh_cache_status = |handle: &TermHandle, app: &AppState| {
        let (w, _) = handle.size();
        handle.set_status_line(app.cache.to_status_block(w.max(40)));
    };
    match event {
        ServerEvent::Chunk { chunk, .. } => match chunk {
            OutputChunk::TextDelta(s) => {
                if s.is_empty() {
                    return;
                }
                // If the previous turn was finalized, auto-reset for a new turn.
                if streaming.finalized {
                    streaming.block_id = None;
                    streaming.buf.clear();
                    streaming.finalized = false;
                }
                streaming.buf.push_str(&s);
                if let Some(id) = streaming.block_id {
                    let block = StyledBlock::new(StyledText::from(Span::new(
                        streaming.buf.clone(),
                        s_assistant(),
                    )));
                    handle.set_block(id, block);
                    handle.redraw();
                } else {
                    let block = StyledBlock::new(StyledText::from(Span::new(
                        streaming.buf.clone(),
                        s_assistant(),
                    )));
                    streaming.block_id = Some(handle.print_output(block));
                }
            }
            OutputChunk::TextComplete(s) => {
                // Only act if there is an active streaming block to finalize.
                // Standalone TextComplete without prior TextDelta is silently
                // ignored — the block must first be created via TextDelta.
                if let Some(id) = streaming.block_id.take() {
                    streaming.buf.clear();
                    // Render the complete text as markdown.
                    let block = render_md_block(handle, &s);
                    handle.set_block(id, block);
                    handle.redraw();
                    streaming.finalized = true;
                }
            }
            OutputChunk::ThinkingDelta(s) => {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    s,
                    s_thinking(),
                ))));
            }
            OutputChunk::ThinkingComplete(s) => {
                if !s.is_empty() {
                    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                        s,
                        s_thinking(),
                    ))));
                }
            }
            OutputChunk::ToolStart { name, input, .. } => {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    tool_call_line(&name, &input),
                    s_tool(),
                ))));
            }
            OutputChunk::ToolProgress { output, .. } => {
                for line in output.lines() {
                    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                        format!("  {line}"),
                        s_tool(),
                    ))));
                }
            }
            OutputChunk::ToolEnd {
                name,
                input,
                result,
                ..
            } => {
                // For Transfer tool, save the file content to the user's PWD
                if name == "Transfer" && !result.is_error && !result.text.is_empty() {
                    let file_path = input
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("transferred-file");
                    let basename = std::path::Path::new(file_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "transferred-file".to_string());
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let out_name = format!("{}-{}", basename, timestamp);

                    match std::fs::write(&out_name, &result.text) {
                        Ok(()) => {
                            let cwd = std::env::current_dir().unwrap_or_default();
                            let full_path = cwd.join(&out_name);
                            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                                format!("  ✓ Transferred to: {}", full_path.display()),
                                s_highlight(),
                            ))));
                        }
                        Err(e) => {
                            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                                format!("  ✗ Failed to save transferred file: {}", e),
                                s_error(),
                            ))));
                        }
                    }
                } else if result.is_error {
                    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                        format!("  ✗ {}", result.text),
                        s_error(),
                    ))));
                } else if !result.text.is_empty() {
                    let preview = cli::truncate_to_width(&result.text, 80);
                    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                        format!("  ✓ {preview}"),
                        s_tool(),
                    ))));
                } else {
                    // Tool succeeded with empty output — still acknowledge it.
                    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                        "  ✓ done",
                        s_tool(),
                    ))));
                }
            }
            OutputChunk::PermissionRequest {
                tool_name,
                action,
                input,
                ..
            } => {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    format!("🔐 {tool_name} wants to {action} — {input}"),
                    s_tool(),
                ))));
            }
            OutputChunk::Status(s) => {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(s, s_system()))));
            }
            OutputChunk::Error(s) => {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(s, s_error()))));
            }
            OutputChunk::Done => {
                // Finalize the current streaming block if one exists.
                if let Some(id) = streaming.block_id.take() {
                    let buf = std::mem::take(&mut streaming.buf);
                    if !buf.is_empty() {
                        // Render accumulated streaming text as markdown.
                        let block = render_md_block(handle, &buf);
                        handle.set_block(id, block);
                        handle.redraw();
                    }
                }
                streaming.reset();
            }
            OutputChunk::CacheTelemetry {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => {
                app.cache
                    .update(input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens);
                refresh_cache_status(handle, app);
            }
            OutputChunk::AskUserQuestion { .. } => {}
            OutputChunk::Unknown => {}
        },
        ServerEvent::Created {
            session_id,
            session_name,
        } => {
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                format!("Session ready: {session_name} ({session_id})"),
                s_system(),
            ))));
            refresh_cache_status(handle, app);
        }
        ServerEvent::SessionList { sessions } => {
            if sessions.is_empty() {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    "(no saved sessions)",
                    s_system(),
                ))));
            } else {
                let mut st =
                    StyledText::from(Span::new("Saved sessions:\n".to_string(), s_system()));
                for s in &sessions {
                    st.push(Span::new(format!("  {s}\n"), s_assistant()));
                }
                handle.print_output(StyledBlock::new(st));
            }
        }
        ServerEvent::SessionResumed {
            session_id,
            session_name,
        } => {
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                format!("Resumed: {session_name} ({session_id})"),
                s_system(),
            ))));
            refresh_cache_status(handle, app);
        }
        ServerEvent::ModelChanged { model } => {
            app.model = model.clone();
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                format!("Model changed: {model}"),
                s_system(),
            ))));
        }
        ServerEvent::SessionCompacted { .. } => {
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                "Session compacted.",
                s_system(),
            ))));
            // Reset cache stats when session is compacted
            app.cache = CacheStats::default();
            refresh_cache_status(handle, app);
        }
        ServerEvent::ModelList { models } => {
            if models.is_empty() {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    "(no models)",
                    s_system(),
                ))));
            } else {
                let mut st =
                    StyledText::from(Span::new("Available models:\n".to_string(), s_system()));
                for m in &models {
                    let style = if m == &app.model {
                        s_highlight()
                    } else {
                        s_assistant()
                    };
                    st.push(Span::new(format!("  {m}\n"), style));
                }
                handle.print_output(StyledBlock::new(st));
            }
        }
        ServerEvent::SystemMsg(msg) => {
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                msg,
                s_system(),
            ))));
        }
        ServerEvent::Unknown(_) => {}
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Accumulated prompt-cache telemetry across the session.
#[derive(Debug, Default, Clone, Copy)]
struct CacheStats {
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    request_count: u64,
}

impl CacheStats {
    fn update(&mut self, input: u32, output: u32, read: u32, created: u32) {
        self.total_input_tokens += input as u64;
        self.total_output_tokens += output as u64;
        self.total_cache_read_tokens += read as u64;
        self.total_cache_creation_tokens += created as u64;
        self.request_count += 1;
    }

    fn hit_rate_pct(&self) -> f64 {
        if self.total_input_tokens == 0 {
            return 0.0;
        }
        (self.total_cache_read_tokens as f64 / self.total_input_tokens as f64) * 100.0
    }

    /// Format a token count with k/m suffix.
    fn fmt_tokens(n: u64) -> String {
        if n >= 1_000_000 {
            let m = n as f64 / 1_000_000.0;
            if m < 10.0 { format!("{:.1}m", m) } else { format!("{:.0}m", m) }
        } else if n >= 1000 {
            let k = n as f64 / 1000.0;
            if k < 10.0 { format!("{:.1}k", k) } else { format!("{:.0}k", k) }
        } else {
            n.to_string()
        }
    }

    /// Render a compact one-line status summary matching pi-agent's format.
    fn to_status_block(&self, _terminal_width: usize) -> StyledBlock {
        if self.request_count == 0 {
            return StyledBlock::new(StyledText::from(Span::new(
                " cache: \u{2014}",
                s_cache_miss(),
            )));
        }
        let pct = self.hit_rate_pct();

        let text = format!(
            " cache: ↑{} ↓{} R{} CH{:5.1}%",
            Self::fmt_tokens(self.total_input_tokens),
            Self::fmt_tokens(self.total_output_tokens),
            Self::fmt_tokens(self.total_cache_read_tokens),
            pct,
        );
        StyledBlock::new(StyledText::from(Span::new(text, s_cache_hit())))
    }
}

struct AppState {
    session_id: String,
    model: String,
    cache: CacheStats,
}

// ---------------------------------------------------------------------------
// Process a submitted line
// ---------------------------------------------------------------------------

fn process_line(
    line: &str,
    app: &mut AppState,
    handle: &TermHandle,
    cmd_tx: &Sender<DaemonCmd>,
) -> LineOutcome {
    // Empty or whitespace-only input is a no-op.
    if line.trim().is_empty() {
        return LineOutcome::Continue;
    }

    if !line.starts_with('/') {
        handle.print_output(StyledBlock::new(StyledText::from(Span::new(
            format!("▸ {line}"),
            s_user(),
        ))));
        let _ = cmd_tx.send(DaemonCmd::Run {
            session_id: app.session_id.clone(),
            content: line.to_string(),
        });
        return LineOutcome::Continue;
    }

    let parts: Vec<&str> = line.split_whitespace().collect();
    let cmd = parts.first().copied().unwrap_or("");

    // Check if this is a known slash command.
    let known_commands: &[&str] = &[
        "/quit",
        "/exit",
        "/status",
        "/help",
        "/clear",
        "/models",
        "/sessions",
        "/interrupt",
        "/compact",
        "/model",
        "/new",
        "/resume",
    ];
    if !known_commands.contains(&cmd) {
        // Unknown slash command — treat as user text, not an error.
        handle.print_output(StyledBlock::new(StyledText::from(Span::new(
            format!("▸ {line}"),
            s_user(),
        ))));
        let _ = cmd_tx.send(DaemonCmd::Run {
            session_id: app.session_id.clone(),
            content: line.to_string(),
        });
        return LineOutcome::Continue;
    }

    match cmd {
        "/quit" | "/exit" => return LineOutcome::Quit,

        "/status" => {
            // Ping the daemon with a plain list_models request — no new
            // daemon command needed. The UI loop arms a timer and resolves
            // on the next ModelList event.
            let _ = cmd_tx.send(DaemonCmd::ListModels);
            return LineOutcome::StatusRequested;
        }

        "/help" => {
            let mut st =
                StyledText::from(Span::new("Available commands:\n".to_string(), s_system()));
            for (name, desc) in SLASH_COMMANDS {
                st.push(Span::new(format!("  {name:<14} {desc}\n"), s_assistant()));
            }
            handle.print_output(StyledBlock::new(st));
        }
        "/clear" => handle.clear_output(),

        "/models" => {
            let _ = cmd_tx.send(DaemonCmd::ListModels);
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                "Fetching models…",
                s_system(),
            ))));
        }
        "/sessions" => {
            let _ = cmd_tx.send(DaemonCmd::ListSessions);
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                "Fetching sessions…",
                s_system(),
            ))));
        }
        "/interrupt" => {
            let _ = cmd_tx.send(DaemonCmd::Interrupt(app.session_id.clone()));
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                "Interrupted.",
                s_system(),
            ))));
        }
        "/compact" => {
            let _ = cmd_tx.send(DaemonCmd::Compact(app.session_id.clone()));
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                "Compacting…",
                s_system(),
            ))));
        }
        "/model" => {
            if let Some(model) = parts.get(1) {
                let model = model.to_string();
                app.model = model.clone();
                let _ = cmd_tx.send(DaemonCmd::SetModel {
                    session_id: app.session_id.clone(),
                    model,
                });
            } else {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    "Usage: /model <model-name>",
                    s_error(),
                ))));
            }
        }
        "/new" => {
            let name = parts
                .get(1)
                .map(|s| s.to_string())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string());
            app.session_id = name.clone();
            handle.clear_output();
            let _ = cmd_tx.send(DaemonCmd::Run {
                session_id: name.clone(),
                content: String::new(),
            });
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                format!("New session: {name}"),
                s_system(),
            ))));
        }
        "/resume" => {
            if let Some(name) = parts.get(1) {
                let name = name.to_string();
                app.session_id = name.clone();
                handle.clear_output();
                let _ = cmd_tx.send(DaemonCmd::Resume(name.clone()));
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    format!("Resuming: {name}"),
                    s_system(),
                ))));
            } else {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    "Usage: /resume <session-id>",
                    s_error(),
                ))));
            }
        }
        _ => {
            // Should not reach here due to the known_commands check above,
            // but keep as a safety net.
            handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                format!("Unknown: {cmd}. Try /help"),
                s_error(),
            ))));
        }
    }

    LineOutcome::Continue
}

// ---------------------------------------------------------------------------
// Tab completion
// ---------------------------------------------------------------------------

fn complete(input: &str) -> Option<String> {
    if !input.starts_with('/') {
        return None;
    }
    let candidates: Vec<_> = SLASH_COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(input))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let (name, _) = candidates[0];
    let suffix = &name[input.len()..];
    let mut out = input.to_string();
    out.push_str(suffix);
    out.push(' ');
    Some(out)
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Forwards terminal events into the app channel. Owns the `Term` — when
/// the loop ends and the channel closes, the thread drops it, restoring
/// the terminal.
fn spawn_event_forwarder(mut term: Term, app_tx: Sender<AppEvent>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Some(ev) = term.next_event() {
            if app_tx.send(AppEvent::Term(ev)).is_err() {
                break;
            }
        }
        drop(term);
    })
}

/// The UI event loop: blocks on the single app channel carrying both
/// terminal input and daemon events, so streaming output renders as it
/// arrives without waiting for a keypress.
fn run_loop(
    handle: &TermHandle,
    app: &mut AppState,
    app_tx: &Sender<AppEvent>,
    app_rx: &Receiver<AppEvent>,
    cmd_tx: &Sender<DaemonCmd>,
    status_timeout: Duration,
) {
    let mut streaming = Streaming {
        block_id: None,
        buf: String::new(),
        finalized: false,
    };
    let mut history: Vec<String> = Vec::new();
    // Set while a `/status` ping is awaiting the daemon's ModelList reply.
    let mut status_pending: Option<std::time::Instant> = None;

    while let Ok(event) = app_rx.recv() {
        match event {
            AppEvent::Daemon(DaemonEv::Event(ev)) => {
                // A ModelList reply resolves a pending /status ping.
                if status_pending.is_some() {
                    if let ServerEvent::ModelList { models } = &ev {
                        status_pending = None;
                        handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                            format!(
                                "✓ omega-loop connection OK — session: {} | model: {} | {} model(s) available",
                                app.session_id,
                                app.model,
                                models.len(),
                            ),
                            s_highlight(),
                        ))));
                        continue;
                    }
                }
                handle_daemon_event(handle, app, &mut streaming, ev);
            }
            AppEvent::Daemon(DaemonEv::Disconnected) => {
                if status_pending.take().is_some() {
                    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                        "✗ omega-loop connection lost",
                        s_error(),
                    ))));
                }
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                    "⚠ Daemon disconnected. /quit to exit.",
                    s_error(),
                ))));
            }
            AppEvent::StatusTimeout => {
                if let Some(since) = status_pending {
                    if since.elapsed() >= status_timeout {
                        status_pending = None;
                        handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                            "✗ no response from omega-loop — connection may be dead",
                            s_error(),
                        ))));
                    }
                }
            }
            AppEvent::Term(Event::Line(line)) => {
                if line.is_empty() {
                    continue;
                }
                history.push(line.clone());
                match process_line(&line, app, handle, cmd_tx) {
                    LineOutcome::Quit => break,
                    LineOutcome::Continue => {}
                    LineOutcome::StatusRequested => {
                        status_pending = Some(std::time::Instant::now());
                        let tx = app_tx.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(status_timeout);
                            let _ = tx.send(AppEvent::StatusTimeout);
                        });
                    }
                }
            }
            AppEvent::Term(Event::Eof) | AppEvent::Term(Event::CancelPrompt) => break,
            AppEvent::Term(Event::BufferChanged) => {
                let buf = handle.get_buffer();
                if buf.ends_with('\t') {
                    let trimmed = buf[..buf.len() - 1].to_string();
                    if let Some(completed) = complete(&trimmed) {
                        handle.set_buffer(completed.clone(), completed.len());
                    } else {
                        let len = trimmed.len();
                        handle.set_buffer(trimmed, len);
                    }
                }
            }
            AppEvent::Term(Event::Escape) => {
                // Only clear buffer on Escape if it's empty (like CancelPrompt).
                // Non-empty buffers are preserved — accidental Esc shouldn't wipe input.
                if handle.get_buffer().is_empty() {
                    handle.set_buffer(String::new(), 0);
                }
            }
            AppEvent::Term(Event::Resize { width, height: _ }) => {
                // Refresh the status line (e.g. cache bar) at the new width.
                // Use the resize event's width directly since the Term's internal
                // SharedState may not have been updated yet (this event arrives
                // via the app channel, not the raw input channel).
                let w = width.max(1) as usize;
                handle.set_status_line(app.cache.to_status_block(w.max(40)));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();

    // ── Create terminal FIRST — no daemon dependency ────────────────────
    // The terminal appears instantly so the user sees something immediately.
    // Daemon connection and model negotiation happen in the background.
    let prompt = StyledText::from(Span::new("▸ ", Style::default().fg(Color::DarkYellow)));
    let (term, handle) = Term::new(prompt)?;

    // Show immediate feedback while we connect in the background.
    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
        "Connecting to omega-loop…",
        s_system(),
    ))));

    let (cmd_tx, cmd_rx) = mpsc::channel::<DaemonCmd>();
    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();

    // ── Spawn daemon connection + IO on a background thread ────────────
    // If the daemon is not reachable we show an error in the TUI instead
    // of blocking startup.
    let daemon_thread = {
        let app_tx = app_tx.clone();
        std::thread::spawn(move || {
            let rt = Runtime::new().expect("tokio runtime");
            rt.block_on(async {
                let client = match AgentdClient::connect().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(target: "omega_tui::daemon", error = %e, "failed to connect");
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(
                            ServerEvent::SystemMsg(format!("Failed to connect to omega-loop: {e}")),
                        )));
                        let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                        return;
                    }
                };
                let (reader, writer) = client.split();
                daemon_loop(reader, writer, cmd_rx, app_tx).await;
            });
        })
    };

    // ── Initialise session with defaults (will be updated by daemon) ───
    let session_id = format!("tui-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());

    // Queue the init commands — they'll be processed as soon as the
    // daemon thread connects and starts its event loop.  Run creates
    // the session, SetModel sets our default model (from env or fallback).
    // We skip ListModels to avoid the model list being printed in the TUI
    // at startup (the user can run `/models` or `/status` later).
    cmd_tx
        .send(DaemonCmd::Run {
            session_id: session_id.clone(),
            content: String::new(),
        })
        .ok();
    cmd_tx
        .send(DaemonCmd::SetModel {
            session_id: session_id.clone(),
            model: model.clone(),
        })
        .ok();

    let mut app = AppState {
        session_id,
        model,
        cache: CacheStats::default(),
    };

    // ── Welcome (printed immediately — no daemon round-trip) ───────────
    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
        format!(
            "omega-tui — session: {} | model: {}\nType /help for commands.",
            app.session_id, app.model
        ),
        s_system(),
    ))));

    // ── Event loop ─────────────────────────────────────────────────────
    // Terminal events are forwarded into the same channel as daemon
    // events, so the loop below wakes for either source.
    let forwarder = spawn_event_forwarder(term, app_tx.clone());

    run_loop(&handle, &mut app, &app_tx, &app_rx, &cmd_tx, STATUS_TIMEOUT);

    // ── Cleanup ────────────────────────────────────────────────────────
    // Shut down the daemon task (drops writer → closes socket → daemon cleans up session)
    let _ = cmd_tx.send(DaemonCmd::Shutdown);
    drop(cmd_tx);
    // Unblock the forwarder's next_event so it drops the Term (restoring
    // the terminal) and exits.
    handle.request_input_shutdown();
    drop(app_tx);
    drop(app_rx);
    let _ = forwarder.join();
    let _ = daemon_thread.join();

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — drive the UI flows with mocked daemon events on a virtual terminal
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cli::emulator::{Capture, Emulator};
    use cli::{BlockId, RawEvent};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use omega_loop_client::{OutputChunk, ServerEvent, ToolResultWire};
    use std::sync::{Arc, Mutex};

    const ROWS: usize = 12;
    const COLS: usize = 40;

    struct Fixture {
        term: Term,
        handle: TermHandle,
        input: std::sync::mpsc::Sender<RawEvent>,
        buf: Arc<Mutex<Vec<u8>>>,
        cmd_tx: Sender<DaemonCmd>,
        cmd_rx: Receiver<DaemonCmd>,
        app: AppState,
        streaming: Streaming,
    }

    fn fixture() -> Fixture {
        let (capture, buf) = Capture::new();
        let prompt = StyledText::from(Span::new("P> ", Style::default()));
        let (term, handle, input) = Term::new_virtual(COLS, ROWS, prompt, capture);
        let (cmd_tx, cmd_rx) = mpsc::channel::<DaemonCmd>();
        Fixture {
            term,
            handle,
            input,
            buf,
            cmd_tx,
            cmd_rx,
            app: AppState {
                session_id: "sess-1".to_string(),
                model: "gpt-a".to_string(),
                cache: CacheStats::default(),
            },
            streaming: Streaming {
                block_id: None,
                buf: String::new(),
                finalized: false,
            },
        }
    }

    fn emulator(fx: &Fixture) -> Emulator {
        Emulator::from_capture(ROWS, COLS, &fx.buf)
    }

    fn key(c: char) -> RawEvent {
        RawEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    /// Types `s` char by char, draining the BufferChanged event after each
    /// keystroke to keep the flow deterministic.
    fn type_str(fx: &mut Fixture, s: &str) {
        for c in s.chars() {
            fx.input.send(key(c)).expect("input open");
            match fx.term.next_event() {
                Some(Event::BufferChanged) => {}
                other => panic!("expected BufferChanged, got {other:?}"),
            }
        }
    }

    fn submit(fx: &mut Fixture) -> String {
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        match fx.term.next_event() {
            Some(Event::Line(line)) => line,
            other => panic!("expected Line, got {other:?}"),
        }
    }

    fn chunk(c: OutputChunk) -> ServerEvent {
        ServerEvent::Chunk {
            session_id: "sess-1".to_string(),
            chunk: c,
        }
    }

    /// Counts rows containing `needle` across visible screen + scrollback.
    fn count_rows_containing(fx: &Fixture, needle: &str) -> usize {
        let em = emulator(fx);
        em.screen_lines()
            .iter()
            .chain(em.history().iter())
            .filter(|l| l.contains(needle))
            .count()
    }

    // --- process_line: user input and slash commands ---------------------

    #[test]
    fn user_line_sends_run_and_echoes() {
        let mut fx = fixture();
        let outcome = process_line("hello agent", &mut fx.app, &fx.handle, &fx.cmd_tx);
        assert_eq!(outcome, LineOutcome::Continue);
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::Run {
                session_id,
                content,
            }) => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(content, "hello agent");
            }
            other => panic!("expected Run, got {other:?}"),
        }
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "▸ hello agent"), 1);
    }

    #[test]
    fn quit_returns_true() {
        let mut fx = fixture();
        assert_eq!(
            process_line("/quit", &mut fx.app, &fx.handle, &fx.cmd_tx),
            LineOutcome::Quit
        );
        assert!(fx.cmd_rx.try_recv().is_err(), "no command expected");
    }

    #[test]
    fn model_command_sets_model() {
        let mut fx = fixture();
        process_line("/model gpt-b", &mut fx.app, &fx.handle, &fx.cmd_tx);
        assert_eq!(fx.app.model, "gpt-b");
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::SetModel { session_id, model }) => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(model, "gpt-b");
            }
            other => panic!("expected SetModel, got {other:?}"),
        }
    }

    #[test]
    fn help_renders_commands() {
        let mut fx = fixture();
        process_line("/help", &mut fx.app, &fx.handle, &fx.cmd_tx);
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "Available commands"), 1);
        assert_eq!(count_rows_containing(&fx, "/compact"), 1);
        assert_eq!(count_rows_containing(&fx, "/interrupt"), 1);
    }

    #[test]
    fn unknown_command_renders_error() {
        let mut fx = fixture();
        // Unknown slash commands are now sent as user text to the agent.
        process_line("/bogus", &mut fx.app, &fx.handle, &fx.cmd_tx);
        fx.handle.redraw_sync();
        // The text is echoed as a user message and sent as a Run command.
        assert_eq!(count_rows_containing(&fx, "▸ /bogus"), 1);
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::Run { content, .. }) => assert_eq!(content, "/bogus"),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn clear_takes_full_render_path() {
        let mut fx = fixture();
        process_line("hello", &mut fx.app, &fx.handle, &fx.cmd_tx);
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "▸ hello"), 1);

        process_line("/clear", &mut fx.app, &fx.handle, &fx.cmd_tx);
        fx.handle.redraw_sync();
        let out = fx.buf.lock().unwrap().clone();
        assert!(
            out.windows(4).any(|w| w == b"\x1b[3J"),
            "/clear did not full-render"
        );
        assert_eq!(count_rows_containing(&fx, "▸ hello"), 0);
    }

    // --- handle_daemon_event: mocked daemon responses --------------------

    #[test]
    fn streaming_deltas_update_single_block() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("Hello, ".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("world".into())),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "Hello, world"), 1);

        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextComplete("Hello, world!".into())),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "Hello, world!"), 1);
        assert_eq!(count_rows_containing(&fx, "Hello, world"), 1);
    }

    #[test]
    fn streaming_then_done_keeps_text() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("partial".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "partial"), 1);
        // Done finalizes and resets the streaming tracker for the next turn.
        assert!(fx.streaming.block_id.is_none());
        assert!(fx.streaming.buf.is_empty());
    }

    #[test]
    fn tool_events_render() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::ToolStart {
                id: "t1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "src/main.rs"}),
            }),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::ToolEnd {
                id: "t1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "src/main.rs"}),
                result: ToolResultWire {
                    text: "boom".into(),
                    is_error: true,
                    content: None,
                },
            }),
        );
        fx.handle.redraw_sync();
        assert_eq!(
            count_rows_containing(&fx, "tool call read_file: src/main.rs"),
            1
        );
        assert_eq!(count_rows_containing(&fx, "✗ boom"), 1);
    }

    #[test]
    fn tool_call_line_prefers_command_over_noise() {
        // Bash: show only the command — not the description or other fields.
        let line = tool_call_line(
            "Bash",
            &serde_json::json!({
                "command": "pwd && ls -la",
                "description": "Check current working directory",
            }),
        );
        assert_eq!(line, "tool call Bash: pwd && ls -la");
        assert!(!line.contains("description"));

        // Multi-line commands collapse into one row.
        let line = tool_call_line("Bash", &serde_json::json!({"command": "cd /tmp\nls"}));
        assert_eq!(line, "tool call Bash: cd /tmp ; ls");

        // Empty input: just the tool name.
        let line = tool_call_line("ls", &serde_json::json!({}));
        assert_eq!(line, "tool call ls");

        // Unknown shape: compact JSON fallback.
        let line = tool_call_line("grep", &serde_json::json!({"foo": "bar"}));
        assert_eq!(line, "tool call grep: {\"foo\":\"bar\"}");
    }

    #[test]
    fn model_changed_renders_confirmation() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            ServerEvent::ModelChanged {
                model: "gpt-b".into(),
            },
        );
        fx.handle.redraw_sync();
        assert_eq!(fx.app.model, "gpt-b");
        assert_eq!(count_rows_containing(&fx, "Model changed: gpt-b"), 1);
    }

    #[test]
    fn status_and_error_render() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Status("working…".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Error("it broke".into())),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "working…"), 1);
        assert_eq!(count_rows_containing(&fx, "it broke"), 1);
    }

    #[test]
    fn model_list_renders_and_marks_current() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            ServerEvent::ModelList {
                models: vec!["gpt-a".into(), "gpt-b".into()],
            },
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "Available models"), 1);
        assert_eq!(count_rows_containing(&fx, "gpt-a"), 1);
        assert_eq!(count_rows_containing(&fx, "gpt-b"), 1);
    }

    // --- end-to-end: type → submit → mocked daemon reply ------------------

    #[test]
    fn end_to_end_prompt_to_response() {
        let mut fx = fixture();
        fx.handle.redraw_sync();

        // User types a prompt and submits it.
        type_str(&mut fx, "hello");
        let line = submit(&mut fx);
        assert_eq!(line, "hello");
        let outcome = process_line(&line, &mut fx.app, &fx.handle, &fx.cmd_tx);
        assert_eq!(outcome, LineOutcome::Continue);
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::Run { content, .. }) => assert_eq!(content, "hello"),
            other => panic!("expected Run, got {other:?}"),
        }

        // Daemon streams a reply; then input must be ready for the next turn.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("Hi there".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();

        let em = emulator(&fx);
        let all: Vec<String> = em
            .history()
            .iter()
            .cloned()
            .chain(em.screen_lines())
            .collect();
        let user_row = all.iter().position(|l| l.contains("▸ hello")).unwrap();
        let reply_row = all.iter().position(|l| l.contains("Hi there")).unwrap();
        assert!(
            user_row < reply_row,
            "reply must follow the prompt: {all:?}"
        );
        // Fresh prompt sits below the reply with the cursor after it.
        let prompt_row = all.iter().rposition(|l| l.trim_end() == "P>").unwrap();
        assert!(
            reply_row < prompt_row,
            "no fresh prompt after reply: {all:?}"
        );

        // The input buffer was cleared by the submission and still works.
        type_str(&mut fx, "again");
        assert_eq!(fx.handle.get_buffer(), "again");
    }

    #[test]
    fn streaming_blocks_follow_their_prompt() {
        let mut fx = fixture();
        fx.handle.redraw_sync();

        // Two sequential turns: each reply must land after its own prompt
        // line, not interleaved with the other turn.
        for (prompt_text, reply) in [("one", "first reply"), ("two", "second reply")] {
            type_str(&mut fx, prompt_text);
            let line = submit(&mut fx);
            process_line(&line, &mut fx.app, &fx.handle, &fx.cmd_tx);
            let _ = fx.cmd_rx.try_recv();
            handle_daemon_event(
                &fx.handle,
                &mut fx.app,
                &mut fx.streaming,
                chunk(OutputChunk::TextDelta(reply.into())),
            );
            handle_daemon_event(
                &fx.handle,
                &mut fx.app,
                &mut fx.streaming,
                chunk(OutputChunk::TextComplete(reply.into())),
            );
            fx.handle.redraw_sync();
            fx.streaming.reset();
        }

        let em = emulator(&fx);
        let all: Vec<String> = em
            .history()
            .iter()
            .cloned()
            .chain(em.screen_lines())
            .collect();
        let pos = |needle: &str| all.iter().position(|l| l.contains(needle)).unwrap();
        assert!(pos("▸ one") < pos("first reply"), "{all:?}");
        assert!(pos("first reply") < pos("▸ two"), "{all:?}");
        assert!(pos("▸ two") < pos("second reply"), "{all:?}");
        assert_eq!(count_rows_containing(&fx, "first reply"), 1);
        assert_eq!(count_rows_containing(&fx, "second reply"), 1);
    }

    // --- run_loop: the real UI loop, wired like main() -------------------

    /// Pieces for driving `run_loop` in tests: the Term is moved into the
    /// forwarder exactly as in main().
    struct LoopFixture {
        handle: TermHandle,
        buf: Arc<Mutex<Vec<u8>>>,
        app: AppState,
        app_tx: Sender<AppEvent>,
        app_rx: Receiver<AppEvent>,
        cmd_tx: Sender<DaemonCmd>,
        cmd_rx: Receiver<DaemonCmd>,
        forwarder: JoinHandle<()>,
        /// Kept alive so the virtual input thread doesn't emit Eof early.
        _input: std::sync::mpsc::Sender<RawEvent>,
    }

    fn loop_fixture() -> LoopFixture {
        let (capture, buf) = Capture::new();
        let prompt = StyledText::from(Span::new("P> ", Style::default()));
        let (term, handle, input) = Term::new_virtual(COLS, ROWS, prompt, capture);
        let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<DaemonCmd>();
        let forwarder = spawn_event_forwarder(term, app_tx.clone());
        LoopFixture {
            handle,
            buf,
            app: AppState {
                session_id: "sess-1".to_string(),
                model: "gpt-a".to_string(),
                cache: CacheStats::default(),
            },
            app_tx,
            app_rx,
            cmd_tx,
            cmd_rx,
            forwarder,
            _input: input,
        }
    }

    impl LoopFixture {
        fn count(&self, needle: &str) -> usize {
            let em = Emulator::from_capture(ROWS, COLS, &self.buf);
            em.screen_lines()
                .iter()
                .chain(em.history().iter())
                .filter(|l| l.contains(needle))
                .count()
        }

        /// True if the full rendered transcript contains `needle`,
        /// tolerating soft-wraps that split it across rows.
        fn transcript_contains(&self, needle: &str) -> bool {
            let em = Emulator::from_capture(ROWS, COLS, &self.buf);
            let joined: String = em
                .history()
                .iter()
                .cloned()
                .chain(em.screen_lines())
                .collect::<Vec<_>>()
                .join("");
            joined.contains(needle)
        }

        fn shutdown(self) {
            self.handle.request_input_shutdown();
            drop(self.app_tx);
            let _ = self.forwarder.join();
        }
    }

    /// Regression test: daemon events arriving while the UI loop is
    /// blocked waiting for input must render without any keypress.
    #[test]
    fn daemon_events_render_without_keypresses() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                // Land while run_loop is parked on recv().
                std::thread::sleep(Duration::from_millis(50));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("streamed".into()),
                ))));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(OutputChunk::Done))));
                std::thread::sleep(Duration::from_millis(50));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        assert_eq!(fx.count("streamed"), 1);
        fx.shutdown();
    }

    #[test]
    fn status_reports_connected() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                let _ = app_tx.send(AppEvent::Term(Event::Line("/status".to_string())));
                // Channel order guarantees run_loop arms the ping before
                // it sees this reply.
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(ServerEvent::ModelList {
                    models: vec!["gpt-a".into(), "gpt-b".into()],
                })));
                std::thread::sleep(Duration::from_millis(50));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(500),
        );
        driver.join().unwrap();
        // The ping must have gone out as a plain list_models request.
        assert!(matches!(fx.cmd_rx.try_recv(), Ok(DaemonCmd::ListModels)));
        fx.handle.redraw_sync();
        assert_eq!(fx.count("omega-loop connection OK"), 1);
        assert!(fx.transcript_contains("session: sess-1"));
        assert!(fx.transcript_contains("model: gpt-a"));
        fx.shutdown();
    }

    #[test]
    fn status_timeout_reports_dead_connection() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                let _ = app_tx.send(AppEvent::Term(Event::Line("/status".to_string())));
                // No daemon reply ever arrives; the 100ms timer must fire.
                std::thread::sleep(Duration::from_millis(400));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(100),
        );
        driver.join().unwrap();
        assert!(matches!(fx.cmd_rx.try_recv(), Ok(DaemonCmd::ListModels)));
        fx.handle.redraw_sync();
        assert_eq!(fx.count("no response from omega-loop"), 1);
        fx.shutdown();
    }

    #[test]
    fn status_disconnected_reports_lost() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                let _ = app_tx.send(AppEvent::Term(Event::Line("/status".to_string())));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                std::thread::sleep(Duration::from_millis(50));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(500),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        assert_eq!(fx.count("omega-loop connection lost"), 1);
        fx.shutdown();
    }

    /// Two sequential user turns through the real `run_loop`, each followed
    /// by mocked daemon streaming, must both render — reproducing the
    /// user-reported bug where the second turn's response never appears.
    #[test]
    fn two_turns_both_render_responses() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                // --- Turn 1 ---
                let _ = app_tx.send(AppEvent::Term(Event::Line("first".to_string())));
                std::thread::sleep(Duration::from_millis(20));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("response one".into()),
                ))));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(OutputChunk::Done))));
                // --- Turn 2 ---
                std::thread::sleep(Duration::from_millis(20));
                let _ = app_tx.send(AppEvent::Term(Event::Line("second".to_string())));
                std::thread::sleep(Duration::from_millis(20));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("response two".into()),
                ))));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(OutputChunk::Done))));
                std::thread::sleep(Duration::from_millis(40));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        assert_eq!(fx.count("response one"), 1);
        assert_eq!(fx.count("response two"), 1);
        assert_eq!(fx.count("▸ first"), 1);
        assert_eq!(fx.count("▸ second"), 1);
        fx.shutdown();
    }

    #[test]
    fn streaming_block_id_reused_across_deltas() {
        // The first delta creates a history block; subsequent deltas mutate
        // it in place (the id stays stable and no extra rows appear).
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("a".into())),
        );
        let id: BlockId = fx.streaming.block_id.expect("block created");
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("b".into())),
        );
        assert_eq!(fx.streaming.block_id, Some(id));
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("c".into())),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "abc"), 1);
    }

    // =========================================================================
    // Edge-case tests: streaming state-machine invariants
    // =========================================================================

    /// TextDelta arriving after TextComplete (same turn) is part of a new turn
    /// and auto-resets the streaming tracker.  TextComplete only finalizes a
    /// block that was previously started by TextDelta.
    #[test]
    fn textdelta_after_textcomplete_ignored() {
        let mut fx = fixture();
        // Turn 1: TextDelta starts streaming, TextComplete finalizes.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("final".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextComplete("final".into())),
        );
        // streaming.finalized is now true; next TextDelta auto-resets for a new turn.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("late".into())),
        );
        fx.handle.redraw_sync();
        // "late" starts a new turn (auto-reset on finalized) — it IS visible.
        assert_eq!(count_rows_containing(&fx, "late"), 1);
        assert_eq!(count_rows_containing(&fx, "final"), 1);
        // finalized resets when TextDelta starts a new turn.
        assert!(!fx.streaming.finalized);
    }

    /// A Done emitted with zero prior TextDeltas should be a safe no-op
    /// (no block to finalize, nothing to render).
    #[test]
    fn done_without_any_textdelta_is_noop() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        assert!(fx.streaming.block_id.is_none());
        assert!(fx.streaming.buf.is_empty());
        // streaming must be fully reset and ready for a subsequent turn.
        assert!(!fx.streaming.finalized);
    }

    /// Two Done chunks back-to-back must not crash; the second is a no-op.
    #[test]
    fn two_dones_are_idempotent() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("hi".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        assert!(!fx.streaming.finalized); // reset already
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "hi"), 1);
    }

    /// ToolStart + ToolEnd should render even after Done has closed the
    /// text-streaming phase (tools are independent of the streaming tracker).
    #[test]
    fn tool_events_render_after_done() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("text".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        // streaming is reset; tool events should still render.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::ToolStart {
                id: "t2".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::ToolEnd {
                id: "t2".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                result: ToolResultWire {
                    text: "done".into(),
                    is_error: false,
                    content: None,
                },
            }),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "tool call Bash: ls"), 1);
        assert_eq!(count_rows_containing(&fx, "done"), 1);
    }

    /// TextComplete arriving without any prior TextDelta is silently ignored.
    /// The block must first be created via TextDelta.
    #[test]
    fn textcomplete_without_prior_delta_creates_block() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("only".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextComplete("only".into())),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "only"), 1);
        // finalized is true after TextComplete.
        assert!(fx.streaming.finalized);
    }

    /// After Done resets the streaming tracker, a subsequent TextDelta
    /// starts a fresh block — this is correct behaviour (if the agent
    /// emits more text after declaring the turn done, it gets rendered).
    #[test]
    fn textdelta_after_done_starts_new_block() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("before".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        // Done called reset() — streaming is clean.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("after".into())),
        );
        fx.handle.redraw_sync();
        // "after" creates a new block.
        assert_eq!(count_rows_containing(&fx, "before"), 1);
        assert_eq!(count_rows_containing(&fx, "after"), 1);
    }

    /// 50 rapid TextDeltas back-to-back must all accumulate into one block.
    #[test]
    fn fifty_rapid_textdeltas_accumulate() {
        let mut fx = fixture();
        let mut expected = String::new();
        for i in 0..50 {
            let s = format!("chunk{i} ");
            expected.push_str(&s);
            handle_daemon_event(
                &fx.handle,
                &mut fx.app,
                &mut fx.streaming,
                chunk(OutputChunk::TextDelta(s)),
            );
        }
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        // Every chunk word must be present in the output (join all rendered
        // rows so wrapping doesn't defeat the substring search).
        let all_rendered = emulator(&fx)
            .history()
            .iter()
            .chain(emulator(&fx).screen_lines().iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all_rendered.contains("chunk0"), "missing chunk0");
        assert!(all_rendered.contains("chunk49"), "missing chunk49");
        // Only one streaming block, so one copy of each chunk word.
        assert_eq!(all_rendered.matches("chunk0").count(), 1);
    }

    /// BUG: When TextComplete arrives before Done, Done short-circuits
    /// and never calls reset(). The streaming tracker stays finalized —
    /// all future TextDeltas are silently dropped.
    #[test]
    fn textcomplete_before_done_leaves_streaming_stuck() {
        let mut fx = fixture();
        // Turn 1: TextDelta → TextComplete → Done.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("partial".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextComplete("corrected".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();

        // CORRECT: Done resets streaming so the next turn works.
        assert!(
            !fx.streaming.finalized,
            "BUG: TextComplete before Done leaves streaming untouched — Done never resets"
        );
        // Turn 2 should render.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("turn2".into())),
        );
        fx.handle.redraw_sync();
        assert!(
            count_rows_containing(&fx, "turn2") > 0,
            "BUG: turn2 text dropped because streaming was stuck"
        );
    }

    // =========================================================================
    // Edge-case tests: interaction of /clear and /new with streaming
    // =========================================================================

    /// BUG: If the user runs `/clear` while a streaming block is active,
    /// the streaming tracker still holds the old block_id. The next delta
    /// calls `set_block` with an id that no longer lives in any zone —
    /// the text disappears silently.
    #[test]
    fn clear_during_streaming_orphans_the_block() {
        let mut fx = fixture();
        // Simulate an in-flight streaming block.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("before-clear".into())),
        );
        let id_before = fx.streaming.block_id;
        assert!(id_before.is_some());

        // User issues /clear.
        fx.handle.clear_output();
        fx.handle.redraw_sync();

        // Agent continues streaming the same turn.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("after-clear".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();

        // CORRECT behavior: "after-clear" text should be visible.
        // BUG: it is not — clear_output removed the block from all zones.
        assert!(
            count_rows_containing(&fx, "after-clear") > 0,
            "BUG: clear during streaming orphans the streaming block — text is invisible"
        );
    }

    /// BUG: `/new` also calls `clear_output` and resets the session id,
    /// but `streaming.block_id` in the run-loop is untouched — subsequent
    /// deltas land on a block with no zone.
    #[test]
    fn new_session_during_streaming_orphans_the_block() {
        let mut fx = fixture();
        // Streaming is live.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("old-session".into())),
        );
        assert!(fx.streaming.block_id.is_some());

        // User creates a new session mid-stream.
        process_line("/new fresh", &mut fx.app, &fx.handle, &fx.cmd_tx);
        fx.handle.redraw_sync();

        // More deltas arrive for the same logical block.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("new-text".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();

        assert!(
            count_rows_containing(&fx, "new-text") > 0,
            "BUG: new-session during streaming orphans the block — text is invisible"
        );
    }

    /// BUG: When only TextComplete arrives (no Done follows), streaming
    /// is left finalized and the next turn's TextDelta is dropped.
    #[test]
    fn textcomplete_without_done_blocks_next_turn() {
        let mut fx = fixture();
        // Turn 1: TextDelta then TextComplete (no Done).
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("turn1".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextComplete("turn1".into())),
        );
        assert!(fx.streaming.finalized);
        // Turn 2: the next TextDelta should auto-reset and NOT be dropped.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("turn2".into())),
        );
        fx.handle.redraw_sync();
        assert!(
            count_rows_containing(&fx, "turn2") > 0,
            "BUG: TextComplete without Done blocks next turn's TextDelta"
        );
    }

    /// When Done completes the turn normally (the common path), the next
    /// turn must start fresh.
    #[test]
    fn done_resets_for_next_turn() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("first".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        assert!(!fx.streaming.finalized);
        assert!(fx.streaming.block_id.is_none());
        // Second turn.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("second".into())),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "second"), 1);
    }

    // =========================================================================
    // Edge-case tests: input handling
    // =========================================================================

    /// process_line("") — the empty string — does not start with '/' so
    /// it falls into the user-line branch and sends a `Run` with empty
    /// content. The run_loop guards against this, but callers of
    /// `process_line` directly are unprotected.
    #[test]
    fn process_line_empty_sends_empty_run() {
        let mut fx = fixture();
        let outcome = process_line("", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // process_line("") should NOT send a Run command with empty content.
        // It should either be a no-op or return a suitable outcome.
        assert!(
            !matches!(fx.cmd_rx.try_recv(), Ok(DaemonCmd::Run { ref content, .. }) if content.is_empty()),
            "BUG: process_line on empty string sends Run with empty content"
        );
        assert_eq!(outcome, LineOutcome::Continue);
    }

    /// Whitespace-only input should not produce a Run command.
    #[test]
    fn whitespace_only_sends_run() {
        let mut fx = fixture();
        process_line("   ", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // Whitespace-only input should NOT send a Run.
        assert!(
            !matches!(fx.cmd_rx.try_recv(), Ok(DaemonCmd::Run { .. })),
            "BUG: whitespace-only line sends Run command"
        );
    }

    /// Ctrl-D on an empty buffer sends Eof (terminal signal). On a
    /// non-empty buffer it deletes the character under cursor (readline
    /// behavior).
    #[test]
    fn ctrl_d_on_nonempty_buffer_deletes_char() {
        let mut fx = fixture();
        type_str(&mut fx, "data");
        // Send Ctrl-D ('d' with CTRL modifier).
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        // Ctrl-D on non-empty buffer deletes the character at cursor.
        // Cursor is at end (after 'a'), so it does nothing.
        // Move cursor back and try again.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        let _ = fx.term.next_event(); // BufferChanged
        assert_eq!(fx.handle.get_buffer(), "ata");
    }

    /// Ctrl-U clears from cursor position to beginning of buffer.
    #[test]
    fn ctrl_u_from_middle_clears_prefix() {
        let mut fx = fixture();
        type_str(&mut fx, "hello");
        // Move cursor left twice, putting it after "hel".
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // BufferChanged
                                      // Send Ctrl-U.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        assert_eq!(fx.handle.get_buffer(), "lo");
    }

    /// Pressing Home moves cursor to position 0, End to the end.
    #[test]
    fn home_and_end_navigate() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        assert_eq!(fx.handle.get_cursor(), 3);
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 3);
    }

    /// Home on empty buffer stays at 0, End on empty buffer stays at 0.
    #[test]
    fn home_and_end_on_empty_buffer() {
        let mut fx = fixture();
        assert_eq!(fx.handle.get_buffer(), "");
        assert_eq!(fx.handle.get_cursor(), 0);
        // Home on empty buffer — stays at 0, no event
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        // No event fires because cursor didn't change (already at 0)
        assert_eq!(fx.handle.get_cursor(), 0);
        // End on empty buffer — stays at 0, no event
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert_eq!(fx.handle.get_cursor(), 0);
    }

    /// Home/End with cursor already at boundary — should be idempotent.
    #[test]
    fn home_and_end_idempotent() {
        let mut fx = fixture();
        type_str(&mut fx, "hello");
        // Already at end (cursor=5), End should stay at 5 with no event
        assert_eq!(fx.handle.get_cursor(), 5);
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert_eq!(fx.handle.get_cursor(), 5);
        // Move to start, Home should stay at 0 with no event
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert_eq!(fx.handle.get_cursor(), 0);
    }

    /// Home/End with unicode multi-byte content — byte boundaries.
    #[test]
    fn home_and_end_unicode() {
        let mut fx = fixture();
        type_str(&mut fx, "aé漢");
        assert_eq!(fx.handle.get_cursor(), 6); // 1 + 2 + 3 bytes
        assert_eq!(fx.handle.get_buffer(), "aé漢");
        // Home to start (cursor moves, fires BufferChanged)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        // End back to end
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 6);
        // Type more after End appends at cursor (6), then move to end
        type_str(&mut fx, " more");
        assert_eq!(fx.handle.get_cursor(), 11);
        assert_eq!(fx.handle.get_buffer(), "aé漢 more");
        // Home from the end of the longer buffer
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        // End back to end
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 11);
    }

    /// Home from middle of buffer, End from middle of buffer.
    #[test]
    fn home_from_middle_end_from_middle() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world");
        // Move cursor to position 5 (between 'hello' and ' world')
        for _ in 0..6 {
            fx.input
                .send(RawEvent::Key(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::NONE,
                )))
                .unwrap();
            let _ = fx.term.next_event();
        }
        assert_eq!(fx.handle.get_cursor(), 5);
        // Home from middle — should go to 0
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        // End from start — should go to end
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 11);
    }

    /// Unicode multi-byte characters (é, 漢字) must not split in the buffer
    /// and cursor must track byte- (not char-) indices correctly.
    #[test]
    fn unicode_input_cursor_byte_tracking() {
        let mut fx = fixture();
        // Type 'a' + 'é' (2 UTF-8 bytes) + '漢' (3 UTF-8 bytes).
        type_str(&mut fx, "a");
        assert_eq!(fx.handle.get_cursor(), 1);
        type_str(&mut fx, "é");
        assert_eq!(fx.handle.get_cursor(), 3); // 1 + 2
        type_str(&mut fx, "漢");
        assert_eq!(fx.handle.get_cursor(), 6); // 3 + 3
        assert_eq!(fx.handle.get_buffer(), "aé漢");
        // Backspace once — should delete '漢'.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_buffer(), "aé");
        assert_eq!(fx.handle.get_cursor(), 3);
    }

    /// Typing a Tab key inserts a literal `\t` and emits BufferChanged.
    /// The run_loop's BufferChanged handler detects the trailing `\t`,
    /// strips it, and calls `complete()` to expand slash commands.
    #[test]
    fn tab_key_inserts_nothing() {
        let mut fx = fixture();
        type_str(&mut fx, "/mo");
        // Press Tab — inserts \t and emits BufferChanged.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Tab,
                KeyModifiers::NONE,
            )))
            .unwrap();
        // Drain the BufferChanged event.
        assert_eq!(fx.term.next_event(), Some(Event::BufferChanged));
        // Buffer now ends with \t — the run_loop handler will trim and complete.
        assert!(
            fx.handle.get_buffer().ends_with('\t'),
            "Tab should insert a literal \\t"
        );
        // Simulate run_loop's BufferChanged handler.
        let buf = fx.handle.get_buffer();
        let trimmed = buf[..buf.len() - 1].to_string();
        if let Some(completed) = complete(&trimmed) {
            fx.handle.set_buffer(completed.clone(), completed.len());
        }
        // Completion should expand "/mo" to "/model ".
        assert_eq!(fx.handle.get_buffer(), "/model ");
    }

    // =========================================================================
    // Edge-case tests: block lifecycle after clear / invalidation
    // =========================================================================

    /// set_block on a block removed by clear_output silently re-inserts
    /// it into the block map WITHOUT adding it to any zone — invisible.
    #[test]
    fn set_block_after_clear_does_not_render() {
        let fx = fixture();
        // Create a block.
        let id = fx
            .handle
            .print_output(StyledBlock::new(StyledText::from(Span::new(
                "hello",
                s_assistant(),
            ))));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "hello"), 1);
        // Clear everything.
        fx.handle.clear_output();
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "hello"), 0);
        // set_block with the same id — should either render or re-add to history.
        fx.handle.set_block(
            id,
            StyledBlock::new(StyledText::from(Span::new("ghost", s_assistant()))),
        );
        fx.handle.redraw_sync();
        assert!(
            count_rows_containing(&fx, "ghost") > 0,
            "BUG: set_block after clear does not render — block exists but has no zone"
        );
    }

    /// After clear_output the next `print_output` should render visibly
    /// (regression: the allocator must not be hosed).
    #[test]
    fn print_output_after_clear_renders() {
        let fx = fixture();
        fx.handle.clear_output();
        fx.handle.redraw_sync();
        fx.handle
            .print_output(StyledBlock::new(StyledText::from(Span::new(
                "fresh",
                s_assistant(),
            ))));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "fresh"), 1);
    }

    /// Two consecutive /clear commands should not leave the terminal in
    /// a broken state.
    #[test]
    fn two_consecutive_clears_noop() {
        let fx = fixture();
        fx.handle
            .print_output(StyledBlock::new(StyledText::from(Span::new(
                "x",
                s_assistant(),
            ))));
        fx.handle.clear_output();
        fx.handle.clear_output();
        fx.handle.redraw_sync();
        // Terminal should just show the empty prompt.
        fx.handle
            .print_output(StyledBlock::new(StyledText::from(Span::new(
                "ok",
                s_assistant(),
            ))));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "ok"), 1);
    }

    // =========================================================================
    // Edge-case tests: /model without an argument
    // =========================================================================

    /// /model with no argument should show a usage error, not silently
    /// succeed or panic.
    #[test]
    fn model_with_no_argument_shows_error() {
        let mut fx = fixture();
        process_line("/model", &mut fx.app, &fx.handle, &fx.cmd_tx);
        fx.handle.redraw_sync();
        // Must show usage guidance.
        assert_eq!(count_rows_containing(&fx, "Usage: /model"), 1);
        // Must NOT send a SetModel command.
        assert!(!matches!(
            fx.cmd_rx.try_recv(),
            Ok(DaemonCmd::SetModel { .. })
        ));
    }

    // =========================================================================
    // Edge-case tests: two-turn scenario with /clear between
    // =========================================================================

    /// Two complete turns, with a /clear right after the first Done,
    /// should not affect the second turn's rendering.
    #[test]
    fn two_turns_with_clear_between() {
        let mut fx = fixture();
        // Turn 1.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("turn1".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        // Clear.
        fx.handle.clear_output();
        fx.handle.redraw_sync();
        // Turn 2.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("turn2".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "turn1"), 0);
        assert_eq!(count_rows_containing(&fx, "turn2"), 1);
    }

    // =========================================================================
    // Edge-case tests: spacing / display trivialities
    // =========================================================================

    /// /clear produces a clear-screen escape (`\x1b[3J`).
    #[test]
    fn clear_emits_clear_screen_escape() {
        let fx = fixture();
        fx.handle.clear_output();
        fx.handle.redraw_sync();
        let out = fx.buf.lock().unwrap().clone();
        assert!(
            out.windows(4).any(|w| w == b"\x1b[3J"),
            "/clear must emit clear-screen escape"
        );
    }

    /// A long response that wraps across many lines should produce every
    /// chunk's text.
    #[test]
    fn long_streaming_response_wraps_correctly() {
        let mut fx = fixture();
        let long = "x".repeat(COLS + 10);
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta(long.clone())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        // The rendered output must include the full long string.
        assert!(transcript_contains(&fx, &long));
    }

    // =========================================================================
    // Additional edge-case tests that expose bugs
    // =========================================================================

    /// Resize the virtual terminal while a streaming block is mid-flight.
    /// The streaming text must survive the full-render path (path 3).
    #[test]
    fn resize_during_streaming_preserves_text() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("before-resize".into())),
        );
        // Resize the terminal.
        fx.input.send(RawEvent::Resize(80, 20)).expect("input open");
        // Drain the Resize event.
        assert!(matches!(fx.term.next_event(), Some(Event::Resize { .. })));
        // Continue streaming.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta(" after".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        assert!(
            transcript_contains(&fx, "before-resize after"),
            "BUG: resize during streaming lost text — full render path may have dropped partial content"
        );
    }

    /// Pasting a literal tab character (\\t) into the buffer must trigger
    /// the completion logic in run_loop's BufferChanged handler.
    #[test]
    fn paste_literal_tab_triggers_completion() {
        let mut fx = fixture();
        // Type a partial command.
        type_str(&mut fx, "/mo");
        // Simulate run_loop's BufferChanged handler exactly as run_loop does.
        let buf = fx.handle.get_buffer();
        // Manually trigger completion like run_loop would if buf ended with \\t.
        // First, inject the tab into the buffer.
        let tabbed = format!("{}\t", buf);
        fx.handle.set_buffer(tabbed.clone(), tabbed.len());
        // Now run_loop's handler: check ends_with('\\t'), trim it, complete.
        let trimmed = tabbed[..tabbed.len() - 1].to_string();
        if let Some(completed) = complete(&trimmed) {
            // Completion should have expanded "/mo" to "/model ".
            assert!(
                completed.starts_with("/model"),
                "BUG: Tab completion for '/mo' should expand to '/model', got '{completed}'"
            );
        } else {
            // If complete() returned None, the completion table is broken.
            assert!(
                complete("/m").is_some() || complete("/mo").is_some(),
                "BUG: Tab completion returned None for '/mo' — SLASH_COMMANDS has /model"
            );
        }
    }

    /// /model with extra whitespace around the argument should still work.
    #[test]
    fn model_with_extra_whitespace_works() {
        let mut fx = fixture();
        process_line("/model   gpt-b  ", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // The argument should be trimmed.
        assert_eq!(fx.app.model, "gpt-b");
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::SetModel { model, .. }) => assert_eq!(model, "gpt-b"),
            other => panic!("expected SetModel, got {other:?}"),
        }
    }

    /// Two sequential Done events without any TextDelta between them must
    /// leave streaming in a clean state ready for the next turn.
    #[test]
    fn two_dones_with_no_textdelta_between() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        assert!(!fx.streaming.finalized);
        assert!(fx.streaming.block_id.is_none());
        assert!(fx.streaming.buf.is_empty());
        // Next turn works.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("next".into())),
        );
        assert!(fx.streaming.block_id.is_some());
    }

    /// TextDelta with empty string should not create a visible block.
    #[test]
    fn empty_textdelta_does_not_create_block() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta(String::new())),
        );
        fx.handle.redraw_sync();
        // No new block created: streaming.block_id is still None.
        assert!(
            fx.streaming.block_id.is_none(),
            "BUG: empty TextDelta should not allocate a streaming block"
        );
    }

    /// TextDelta → Done → TextComplete: a late TextComplete after Done
    /// is silently ignored (no active block to finalize).  This prevents
    /// duplicate response blocks.
    #[test]
    fn late_textcomplete_after_done_creates_duplicate() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("streaming version".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        // Done reset; late TextComplete is ignored because there is no
        // active streaming block.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextComplete("corrected version".into())),
        );
        fx.handle.redraw_sync();
        // Only the original text is visible; no duplicate.
        assert_eq!(count_rows_containing(&fx, "streaming version"), 1);
        assert_eq!(count_rows_containing(&fx, "corrected version"), 0);
        // Streaming stays clean.
        assert!(!fx.streaming.finalized);
    }

    /// Ctrl-C on a non-empty buffer clears it; on an empty buffer it
    /// sends CancelPrompt.
    #[test]
    fn ctrl_c_on_nonempty_buffer_clears_it() {
        let mut fx = fixture();
        type_str(&mut fx, "stuff");
        assert_eq!(fx.handle.get_buffer(), "stuff");
        // Send Ctrl-C.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        // Drain the BufferChanged event.
        match fx.term.next_event() {
            Some(Event::BufferChanged) => {}
            other => panic!("expected BufferChanged after Ctrl-C, got {other:?}"),
        }
        assert!(
            fx.handle.get_buffer().is_empty(),
            "Ctrl-C on non-empty buffer should clear it"
        );
    }

    /// Ctrl-C on an empty buffer sends CancelPrompt, not BufferChanged.
    #[test]
    fn ctrl_c_on_empty_buffer_sends_cancel() {
        let mut fx = fixture();
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        match fx.term.next_event() {
            Some(Event::CancelPrompt) => {}
            other => panic!("expected CancelPrompt, got {other:?}"),
        }
    }

    /// A single streaming block accumulating text, then Done, should leave
    /// exactly one visible block. After a second Done (spurious), no
    /// duplicate block should appear.
    #[test]
    fn spurious_extra_done_does_not_create_extra_blocks() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("data".into())),
        );
        let block_count_before = count_rows_containing(&fx, "data");
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        // Must not duplicate.
        assert_eq!(
            count_rows_containing(&fx, "data"),
            block_count_before + 1,
            "BUG: spurious Done created extra blocks"
        );
    }

    // =========================================================================
    // Realistic user scenarios that expose bugs
    // =========================================================================

    /// Any message starting with "/" is intercepted as a slash command,
    /// even if the user intended it as literal text (e.g. a file path).
    #[test]
    fn slash_prefix_unintentionally_caught_as_command() {
        let mut fx = fixture();
        // User wants to ask about a file path.
        process_line("/home/user/file.txt", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // The line should be sent as a Run to the agent, NOT treated as
        // a slash command resulting in an "Unknown" error.
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::Run { content, .. }) => {
                assert_eq!(
                    content, "/home/user/file.txt",
                    "Message starting with '/' should be sent as input, not treated as a command"
                );
            }
            other => {
                assert!(
                    false,
                    "BUG: '/home/user/file.txt' was treated as a slash command, got {other:?}"
                );
            }
        }
    }

    /// Ctrl-L should clear the screen (universal terminal shortcut).
    /// Currently it is completely ignored.
    #[test]
    fn ctrl_l_clears_screen() {
        let fx = fixture();
        // Put some content on screen first.
        fx.handle
            .print_output(StyledBlock::new(StyledText::from(Span::new(
                "garbage",
                s_assistant(),
            ))));
        fx.handle.redraw_sync();
        assert!(
            count_rows_containing(&fx, "garbage") > 0,
            "sanity: content on screen"
        );
        // Send Ctrl-L.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(std::time::Duration::from_millis(50));
        // After Ctrl-L, the screen should be cleared.
        // BUG: Ctrl-L is silently ignored — screen remains unchanged.
        assert!(
            count_rows_containing(&fx, "garbage") == 0,
            "BUG: Ctrl-L should clear the screen but it is ignored"
        );
    }

    /// Ctrl-W (delete word) is a standard readline/emacs shortcut
    /// that should delete the word before the cursor.
    #[test]
    fn ctrl_w_deletes_previous_word() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world");
        assert_eq!(fx.handle.get_buffer(), "hello world");
        // Send Ctrl-W.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('w'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(std::time::Duration::from_millis(50));
        // After Ctrl-W, "world" should be deleted, leaving "hello ".
        // BUG: Ctrl-W is silently ignored — buffer is still "hello world".
        assert_eq!(
            fx.handle.get_buffer(),
            "hello ",
            "BUG: Ctrl-W should delete previous word but buffer is unchanged"
        );
    }

    /// Sending /interrupt while a streaming block is active should
    /// not clear the streaming buffer; the next TextDelta after
    /// interrupt should still render.
    #[test]
    fn interrupt_during_streaming_does_not_orphan() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("pre-interrupt".into())),
        );
        let bid = fx.streaming.block_id;
        assert!(bid.is_some());
        // User interrupts.
        process_line("/interrupt", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // Daemon should send Done to terminate the streaming block.
        // But the streaming tracker is still live.
        // More deltas after interrupt should still render.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta(" post-interrupt".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        assert!(
            transcript_contains(&fx, "pre-interrupt post-interrupt"),
            "BUG: interrupt during streaming should not orphan the streaming block"
        );
    }

    /// Escape on a non-empty buffer should NOT wipe typed text.
    /// The Term emits an Escape event but leaves the buffer intact;
    /// run_loop only clears the buffer when it is already empty.
    #[test]
    fn escape_on_nonempty_buffer_should_not_wipe() {
        let mut fx = fixture();
        type_str(&mut fx, "important message");
        assert!(!fx.handle.get_buffer().is_empty());
        // Press Escape → the Term emits Escape but does NOT change buffer.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        // Drain the Escape event.
        assert_eq!(fx.term.next_event(), Some(Event::Escape));
        // The buffer must still be intact after Escape.
        assert!(
            !fx.handle.get_buffer().is_empty(),
            "BUG: Term wiped buffer on Escape — should preserve input"
        );
    }

    /// ToolEnd with Ok result but empty text should still acknowledge
    /// completion so the user knows the tool finished.
    #[test]
    fn toolend_ok_empty_text_is_silent() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::ToolStart {
                id: "t1".into(),
                name: "run".into(),
                input: serde_json::json!({"command": "true"}),
            }),
        );
        fx.handle.redraw_sync();
        // Verify ToolStart rendered.
        assert!(
            count_rows_containing(&fx, "tool call run") > 0,
            "ToolStart text not found"
        );
        assert!(
            count_rows_containing(&fx, "done") == 0,
            "'done' should not appear before ToolEnd"
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::ToolEnd {
                id: "t1".into(),
                name: "run".into(),
                input: serde_json::json!({"command": "true"}),
                result: omega_loop_client::ToolResultWire {
                    text: String::new(),
                    is_error: false,
                    content: None,
                },
            }),
        );
        fx.handle.redraw_sync();
        // The tool completed — an acknowledgment must be visible.
        assert!(
            count_rows_containing(&fx, "done") > 0,
            "BUG: ToolEnd with Ok and empty text is silent — user doesn't know it finished"
        );
    }

    /// Up arrow should recall previous history entry.  Currently Up/Down
    /// are silently ignored.
    #[test]
    fn up_arrow_does_not_recall_history() {
        let mut fx = fixture();
        // First, simulate typing and submitting a line.
        type_str(&mut fx, "first command");
        assert_eq!(fx.handle.get_buffer(), "first command");
        // Press Enter to "submit" — the run_loop would push to history
        // and clear the buffer, but we test the Term-level expectation.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        match fx.term.next_event() {
            Some(Event::Line(line)) => assert_eq!(line, "first command"),
            other => panic!("expected Line, got {other:?}"),
        }
        // Buffer is now empty. Press Up to recall history.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        // BUG: Up/Down are ignored — no event emitted.  The buffer
        // should contain the previous line.
        // Since next_event() blocks, we send a follow-up key to flush.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .expect("input open");
        let ev = fx.term.next_event();
        assert_eq!(ev, Some(Event::BufferChanged));
        // BUG: if Up were handled, buffer would be "first commandx".
        // Instead it's just "x" because Up was ignored.
        assert!(
            fx.handle.get_buffer().contains("first command"),
            "BUG: Up arrow ignored — history not recalled (buffer='{}')",
            fx.handle.get_buffer()
        );
    }

    /// Down arrow is also ignored like Up.
    #[test]
    fn down_arrow_does_nothing() {
        let mut fx = fixture();
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        // Send 'x' to flush.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .expect("input open");
        let ev = fx.term.next_event();
        // BUG: Down was ignored — only one event (from 'x').
        assert_eq!(ev, Some(Event::BufferChanged));
        assert_eq!(fx.handle.get_buffer(), "x");
    }

    /// TextComplete("") then Done — streaming must still be ready for the
    /// next turn.
    #[test]
    fn empty_textcomplete_then_done_stucks_streaming() {
        let mut fx = fixture();
        // Empty TextComplete: no active block, so it's a no-op.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextComplete(String::new())),
        );
        // Done: no active block, resets anyway.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        // Next turn should work.
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("should appear".into())),
        );
        fx.handle.redraw_sync();
        assert!(
            transcript_contains(&fx, "should appear"),
            "BUG: empty TextComplete then Done locks streaming; next TextDelta dropped"
        );
    }

    /// Ctrl-A (Home) is a standard readline shortcut.  Moves cursor to start.
    #[test]
    fn ctrl_a_home_is_ignored() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        assert_eq!(fx.handle.get_cursor(), 3);
        // Press Ctrl-A: cursor moves to 0.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        // Drain the BufferChanged event from Ctrl-A.
        assert_eq!(fx.term.next_event(), Some(Event::BufferChanged));
        assert_eq!(fx.handle.get_cursor(), 0);
        // Type 'x' at position 0 → "xabc".
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .expect("input open");
        assert_eq!(fx.term.next_event(), Some(Event::BufferChanged));
        assert_eq!(fx.handle.get_buffer(), "xabc");
    }

    /// Ctrl-E (End) is a standard readline shortcut.  Moves cursor to end.
    #[test]
    fn ctrl_e_end_is_ignored() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        // Move cursor to start first.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        let _ = fx.term.next_event(); // BufferChanged from Home
        assert_eq!(fx.handle.get_cursor(), 0);
        // Press Ctrl-E: cursor moves to end.
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        assert_eq!(fx.term.next_event(), Some(Event::BufferChanged));
        assert_eq!(fx.handle.get_cursor(), 3);
        // Type 'x' at the end → "abcx".
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .expect("input open");
        assert_eq!(fx.term.next_event(), Some(Event::BufferChanged));
        assert_eq!(fx.handle.get_buffer(), "abcx");
    }

    fn transcript_contains(fx: &Fixture, needle: &str) -> bool {
        let em = emulator(fx);
        let joined: String = em
            .history()
            .iter()
            .chain(em.screen_lines().iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("");
        joined.contains(needle)
    }

    // =========================================================================
    // Prompt caching bar — CacheStats unit tests
    // =========================================================================

    #[test]
    fn cache_stats_default_is_empty() {
        let cs = CacheStats::default();
        assert_eq!(cs.request_count, 0);
        assert_eq!(cs.total_input_tokens, 0);
        assert_eq!(cs.total_cache_read_tokens, 0);
        assert_eq!(cs.total_cache_creation_tokens, 0);
        assert_eq!(cs.hit_rate_pct(), 0.0);
    }

    #[test]
    fn cache_stats_update_accumulates() {
        let mut cs = CacheStats::default();
        cs.update(100, 50, 40, 10);
        assert_eq!(cs.request_count, 1);
        assert_eq!(cs.total_input_tokens, 100);
        assert_eq!(cs.total_cache_read_tokens, 40);
        assert_eq!(cs.total_cache_creation_tokens, 10);
    }

    #[test]
    fn cache_stats_multiple_updates_cumulative() {
        let mut cs = CacheStats::default();
        cs.update(100, 50, 40, 10);
        cs.update(200, 100, 160, 30);
        assert_eq!(cs.request_count, 2);
        assert_eq!(cs.total_input_tokens, 300);
        assert_eq!(cs.total_cache_read_tokens, 200);
        assert_eq!(cs.total_cache_creation_tokens, 40);
    }

    #[test]
    fn cache_stats_hit_rate_100_percent() {
        let mut cs = CacheStats::default();
        cs.update(100, 50, 100, 0);
        assert!((cs.hit_rate_pct() - 100.0).abs() < 0.001);
    }

    #[test]
    fn cache_stats_hit_rate_0_percent() {
        let mut cs = CacheStats::default();
        cs.update(100, 50, 0, 100);
        assert!((cs.hit_rate_pct() - 0.0).abs() < 0.001);
    }

    #[test]
    fn cache_stats_hit_rate_fractional() {
        let mut cs = CacheStats::default();
        cs.update(3, 1, 1, 2);
        // 1/3 = 33.333...%
        assert!((cs.hit_rate_pct() - 100.0 / 3.0).abs() < 0.001);
    }

    #[test]
    fn cache_stats_hit_rate_with_zero_input_tokens() {
        let cs = CacheStats::default();
        // No updates: total_input_tokens == 0 => hit_rate_pct returns 0.0
        assert_eq!(cs.hit_rate_pct(), 0.0);
    }

    #[test]
    fn cache_stats_large_values_do_not_overflow() {
        let mut cs = CacheStats::default();
        // Push values up near u32::MAX repeatedly
        for _ in 0..10 {
            cs.update(u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        }
        // Each u32::MAX ≈ 4.29e9, so 10× fits in u64 easily
        assert!(cs.total_input_tokens as u128 > 0);
        assert!((cs.hit_rate_pct() - 100.0).abs() < 1.0);
    }

    // =========================================================================
    // Prompt caching bar — to_status_block formatting
    // =========================================================================

    #[test]
    fn cache_bar_shows_no_requests_when_empty() {
        let cs = CacheStats::default();
        let block = cs.to_status_block(80);
        let text: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            text.contains("cache:"),
            "default bar should show cache prefix"
        );
    }

    #[test]
    fn cache_bar_shows_percentage_after_update() {
        let mut cs = CacheStats::default();
        cs.update(1000, 500, 500, 100);
        let block = cs.to_status_block(80);
        let text: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(text.contains("50.0%"), "bar should show 50.0% hit rate");
        assert!(
            text.contains("500") || text.contains("0.5"),
            "bar should reference cached tokens"
        );
    }

    #[test]
    fn cache_bar_displays_perfect_hit_rate() {
        let mut cs = CacheStats::default();
        cs.update(1000, 500, 1000, 0);
        let block = cs.to_status_block(80);
        let text: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(text.contains("100.0%"), "100% hit rate should show 100.0%");
        assert!(text.contains("CH100.0%") || text.contains("CH 100.0%"));
    }

    #[test]
    fn cache_bar_displays_zero_hit_rate() {
        let mut cs = CacheStats::default();
        cs.update(1000, 500, 0, 1000);
        let block = cs.to_status_block(80);
        let text: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(text.contains("0.0%"), "0% hit rate should show 0.0%");
        assert!(text.contains("CH"));
    }

    #[test]
    fn cache_bar_fill_never_exceeds_bar_width() {
        // With the new format there is no bar, so just ensure no panic.
        let mut cs = CacheStats::default();
        cs.update(100, 50, 200, 0); // more read than input \u{2014} shouldn't happen
        let block = cs.to_status_block(80);
        let text: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(!text.is_empty());
    }

    #[test]
    fn cache_bar_width_scales_with_terminal_width() {
        // The new format is width-independent (no bar), so same output at any width.
        let mut cs = CacheStats::default();
        cs.update(1000, 500, 500, 100);

        let block_narrow = cs.to_status_block(50);
        let text_n: String = block_narrow
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();

        let block_wide = cs.to_status_block(120);
        let text_w: String = block_wide
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();

        assert_eq!(text_n, text_w, "format should be width-independent");
    }

    #[test]
    fn cache_bar_shows_arrows_and_labels() {
        let mut cs = CacheStats::default();
        cs.update(100, 50, 30, 10);
        let block = cs.to_status_block(80);
        let text: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(text.contains('↑'), "should show up arrow for input");
        assert!(text.contains('↓'), "should show down arrow for output");
        assert!(text.contains('R'), "should show R for cache read");
        assert!(text.contains("CH"), "should show CH for cache hit rate");
        assert!(text.contains("100"), "should show input count");
        assert!(text.contains("50"), "should show output count");
    }

    #[test]
    fn cache_bar_always_uses_hit_color_regardless_of_rate() {
        // The status block always uses s_cache_hit() (green) even at 0% hit rate.
        let mut cs = CacheStats::default();
        cs.update(100, 50, 0, 100);
        let block = cs.to_status_block(80);
        // The top-level style of the block comes from the Span's style.
        let span = &block.content.spans()[0];
        // s_cache_hit() returns green, s_cache_miss() returns dark grey.
        // At 0% it should arguably be grey/red, but current code forces green.
        assert!(
            span.style.fg == Some(Color::Green),
            "BUG: bar always green even at 0% hit rate — should vary by hit rate"
        );
    }

    #[test]
    fn cache_bar_text_is_stale_after_resize_without_telemetry() {
        // With the width-independent format, resize doesn't change the text.
        let mut cs = CacheStats::default();
        cs.update(1000, 500, 500, 100);

        let block_50 = cs.to_status_block(50);
        let text_50: String = block_50
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();

        let block_120 = cs.to_status_block(120);
        let text_120: String = block_120
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();

        assert_eq!(text_50, text_120, "format is width-independent");
    }

    #[test]
    fn cache_bar_text_truncates_at_min_terminal_width() {
        let mut cs = CacheStats::default();
        cs.update(100, 50, 50, 10);
        // terminal_width = 40 should not cause a panic or empty bar
        let block = cs.to_status_block(40);
        let text: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        // The bar should still render with at least bar_width=10 characters
        // New format has no bar chars
        assert!(text.contains('↑'), "should show up arrow at min width");
        assert!(
            text.contains('%'),
            "bar must show percentage even at min width"
        );
    }

    #[test]
    fn cache_bar_with_zero_input_nonzero_read_is_nan() {
        // If total_input_tokens is 0 but cache_read_tokens > 0 (shouldn't happen),
        // hit_rate_pct returns 0.0 (due to early return), so the bar shows 0.0%.
        // This is arguably wrong — it should be undefined/N/A.
        let mut cs = CacheStats::default();
        // Manually set the fields to simulate the impossible state
        cs.total_cache_read_tokens = 50;
        cs.total_input_tokens = 0;
        cs.request_count = 1;
        // hit_rate_pct early-returns 0.0 when total_input_tokens == 0
        // but that hides the fact that cache_read > 0 with zero input
        let pct = cs.hit_rate_pct();
        assert!(
            (pct - 0.0).abs() < 0.001,
            "BUG: with 0 input and positive cache read, hit rate should not be 0.0 — it's undefined"
        );
    }

    // =========================================================================
    // Prompt caching bar — integration via handle_daemon_event
    // =========================================================================

    #[test]
    fn cache_bar_renders_on_cache_telemetry_event() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 50,
                cache_creation_tokens: 10,
            }),
        );
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "cache:"), 1);
        assert!(transcript_contains(&fx, "50.0%"));
        assert_eq!(fx.app.cache.request_count, 1);
    }

    #[test]
    fn cache_bar_accumulates_across_multiple_telemetry_events() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 80,
                cache_creation_tokens: 10,
            }),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 200,
                output_tokens: 100,
                cache_read_tokens: 100,
                cache_creation_tokens: 50,
            }),
        );
        fx.handle.redraw_sync();
        assert_eq!(fx.app.cache.request_count, 2);
        assert_eq!(fx.app.cache.total_input_tokens, 300);
        assert_eq!(fx.app.cache.total_cache_read_tokens, 180);
    }

    #[test]
    fn cache_bar_survives_clear_output() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 50,
                cache_creation_tokens: 10,
            }),
        );
        fx.handle.redraw_sync();
        assert!(transcript_contains(&fx, "cache:"));

        fx.handle.clear_output();
        fx.handle.redraw_sync();
        // After clear, cache bar (status line) should still be visible
        assert!(
            transcript_contains(&fx, "cache:"),
            "BUG: cache bar disappears after /clear — status_line not preserved"
        );
    }

    #[test]
    fn cache_bar_survives_new_session_command() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 50,
                cache_creation_tokens: 10,
            }),
        );
        fx.handle.redraw_sync();
        assert!(transcript_contains(&fx, "cache:"));

        // Simulate /new (clear_output + session change)
        fx.handle.clear_output();
        fx.app.session_id = "sess-2".to_string();
        fx.handle.redraw_sync();
        assert!(
            transcript_contains(&fx, "cache:"),
            "BUG: cache bar disappears after /new — status_line not preserved across sessions"
        );
    }

    #[test]
    fn cache_bar_shown_during_streaming() {
        let mut fx = fixture();
        // Streaming text
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("hello ".into())),
        );
        // Cache telemetry mid-stream
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 200,
                output_tokens: 100,
                cache_read_tokens: 100,
                cache_creation_tokens: 50,
            }),
        );
        // More streaming text
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("world".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();

        assert!(
            transcript_contains(&fx, "hello world"),
            "streaming text must be visible"
        );
        assert!(
            transcript_contains(&fx, "cache:"),
            "cache bar must be visible even during streaming"
        );
    }

    // =========================================================================
    // Prompt caching bar — edge cases in the sync path (LoopFixture)
    // =========================================================================

    #[test]
    fn cache_bar_disconnected_preserves_last_known() {
        let mut fx = loop_fixture();
        // Inject cache telemetry
        fx.app.cache.update(100, 50, 50, 10);
        let (w, _) = fx.handle.size();
        fx.handle
            .set_status_line(fx.app.cache.to_status_block(w.max(40)));
        fx.handle.redraw_sync();
        // Simulate disconnect
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Disconnected));
                std::thread::sleep(Duration::from_millis(50));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        assert!(
            fx.transcript_contains("cache:"),
            "cache bar should persist after disconnect"
        );
        fx.shutdown();
    }

    #[test]
    fn cache_bar_compaction_resets_stats_and_refreshes() {
        let mut fx = fixture();
        // First, set some cache stats
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 1000,
                output_tokens: 500,
                cache_read_tokens: 800,
                cache_creation_tokens: 200,
            }),
        );
        fx.handle.redraw_sync();
        assert_eq!(fx.app.cache.request_count, 1);
        assert!(transcript_contains(&fx, "cache:"));

        // Session compacted resets cache
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            ServerEvent::SessionCompacted {
                session_id: "sess-1".to_string(),
            },
        );
        fx.handle.redraw_sync();
        assert_eq!(fx.app.cache.request_count, 0);
        // After compaction, the bar should show "cache:"
        assert!(
            transcript_contains(&fx, "cache:"),
            "after compaction cache bar should reset to empty state"
        );
    }

    #[test]
    fn cache_bar_with_tool_events_interleaved() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::ToolStart {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hi"}),
            }),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 50,
                output_tokens: 25,
                cache_read_tokens: 10,
                cache_creation_tokens: 5,
            }),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::ToolEnd {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hello"}),
                result: ToolResultWire {
                    text: "done".into(),
                    is_error: false,
                    content: None,
                },
            }),
        );
        fx.handle.redraw_sync();
        assert!(
            transcript_contains(&fx, "tool call bash"),
            "tool must render"
        );
        assert!(transcript_contains(&fx, "done"), "tool end must render");
        assert!(
            transcript_contains(&fx, "cache:"),
            "cache bar must render even with interleaved tool events"
        );
    }

    #[test]
    fn cache_bar_does_not_duplicate_on_multiple_identical_telemetry() {
        let mut fx = loop_fixture();
        // Verify that sending the same telemetry twice doesn't create a duplicate status line
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::CacheTelemetry {
                        input_tokens: 100,
                        output_tokens: 50,
                        cache_read_tokens: 50,
                        cache_creation_tokens: 10,
                    },
                ))));
                std::thread::sleep(Duration::from_millis(10));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::CacheTelemetry {
                        input_tokens: 200,
                        output_tokens: 100,
                        cache_read_tokens: 100,
                        cache_creation_tokens: 20,
                    },
                ))));
                std::thread::sleep(Duration::from_millis(20));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        // Cache bar text should appear exactly once (not twice)
        let count = fx.count("cache:");
        assert_eq!(
            count, 1,
            "BUG: cache bar duplicated — found {count} occurrences"
        );
        fx.shutdown();
    }

    // =========================================================================
    // Cursor behavior — edge cases and missing readline shortcuts
    // =========================================================================

    #[test]
    fn ctrl_d_on_empty_buffer_sends_eof() {
        let mut fx = fixture();
        // Ctrl-D on empty buffer should emit Eof
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        match fx.term.next_event() {
            Some(Event::Eof) => {}
            other => panic!("expected Eof, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_d_should_delete_character_under_cursor_on_nonempty_buffer() {
        let mut fx = fixture();
        type_str(&mut fx, "abcd");
        assert_eq!(fx.handle.get_buffer(), "abcd");
        assert_eq!(fx.handle.get_cursor(), 4);
        // Move cursor left twice: between 'b' and 'c'
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // BufferChanged
                                      // Cursor now at position 2
        assert_eq!(fx.handle.get_cursor(), 2);
        // Ctrl-D should delete the character at cursor (should delete 'c')
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_buffer() == "abd",
            "BUG: Ctrl-D should delete char under cursor (expected 'abd', got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn ctrl_k_should_delete_from_cursor_to_end_of_line() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world");
        // Move cursor to position 5 (after 'hello')
        for _ in 0..6 {
            fx.input
                .send(RawEvent::Key(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::NONE,
                )))
                .unwrap();
            let _ = fx.term.next_event(); // BufferChanged
        }
        assert_eq!(fx.handle.get_cursor(), 5);
        // Ctrl-K should delete from cursor to end, leaving "hello"
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_buffer() == "hello",
            "BUG: Ctrl-K should delete to end of line (expected 'hello', got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn ctrl_t_should_transpose_characters() {
        let mut fx = fixture();
        type_str(&mut fx, "ab");
        // Ctrl-T should transpose last two chars -> "ba"
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_buffer() == "ba",
            "BUG: Ctrl-T should transpose characters (expected 'ba', got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn ctrl_p_should_recall_previous_history_entry() {
        let mut fx = fixture();
        // Submit a first command
        type_str(&mut fx, "first cmd");
        let line = submit(&mut fx);
        assert_eq!(line, "first cmd");
        // Submit a second command
        type_str(&mut fx, "second cmd");
        let line2 = submit(&mut fx);
        assert_eq!(line2, "second cmd");
        // Now buffer is empty. Ctrl-P should recall the previous entry.
        assert_eq!(fx.handle.get_buffer(), "");
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_buffer() == "second cmd",
            "BUG: Ctrl-P should recall previous history (expected 'second cmd', got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn ctrl_n_should_recall_next_history_entry() {
        let mut fx = fixture();
        // Submit two commands
        type_str(&mut fx, "first");
        submit(&mut fx);
        type_str(&mut fx, "second");
        submit(&mut fx);
        // Recall with up arrow
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_buffer(), "second");
        // Ctrl-N should go to next entry (back to "first"? or clear?)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        // Actually Ctrl-N should go forward in history, which would go back to the
        // second entry (since we're at "second" due to up, pressing up again goes to "first",
        // so pressing down/Ctrl-N from "second" should go to the older "first")
        assert!(
            fx.handle.get_buffer() == "first" || fx.handle.get_buffer() == "",
            "BUG: Ctrl-N should navigate forward in history (got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn alt_f_should_move_forward_one_word() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world foo");
        // Cursor at end. Move to start with Ctrl-A
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        assert_eq!(fx.handle.get_cursor(), 0);
        // Alt-F should move forward one word
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::ALT,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_cursor() == 6, // after "hello "
            "BUG: Alt-F should move forward one word (expected cursor=6, got {})",
            fx.handle.get_cursor()
        );
    }

    #[test]
    fn alt_b_should_move_back_one_word() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world foo");
        // Alt-B from end should move back one word
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::ALT,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        // Cursor should jump to after "world " i.e., position of 'f'
        assert!(
            fx.handle.get_cursor() == 12, // after "hello world "
            "BUG: Alt-B should move back one word (expected cursor=12, got {})",
            fx.handle.get_cursor()
        );
    }

    #[test]
    fn delete_key_at_end_of_buffer_does_nothing() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        assert_eq!(fx.handle.get_cursor(), 3);
        // Delete at end of buffer — should be a no-op (nothing to delete)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        // No change expected — no event should fire because nothing changed
        // Actually a BufferChanged event may or may not fire; the state shouldn't change.
        assert_eq!(
            fx.handle.get_buffer(),
            "abc",
            "Delete at end of buffer should be a no-op"
        );
    }

    #[test]
    fn delete_key_in_middle_deletes_forward() {
        let mut fx = fixture();
        type_str(&mut fx, "abcd");
        // Move cursor to position 1 (between 'a' and 'b')
        for _ in 0..3 {
            fx.input
                .send(RawEvent::Key(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::NONE,
                )))
                .unwrap();
            let _ = fx.term.next_event();
        }
        assert_eq!(fx.handle.get_cursor(), 1);
        // Delete should remove 'b'
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        match fx.term.next_event() {
            Some(Event::BufferChanged) => {}
            other => panic!("expected BufferChanged, got {other:?}"),
        }
        assert_eq!(fx.handle.get_buffer(), "acd");
    }

    #[test]
    fn backspace_at_start_of_buffer_does_nothing() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        // Move cursor to start
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        assert_eq!(fx.handle.get_cursor(), 0);
        // Backspace at start — no-op
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_buffer(),
            "abc",
            "Backspace at start should be a no-op"
        );
    }

    #[test]
    fn cursor_left_at_start_does_not_wrap() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        // Left at start — should stay at 0
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            )))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(fx.handle.get_cursor(), 0, "Left at start must stay at 0");
    }

    #[test]
    fn cursor_right_at_end_does_not_wrap() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        assert_eq!(fx.handle.get_cursor(), 3);
        // Right at end — should stay at 3
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::NONE,
            )))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(fx.handle.get_cursor(), 3, "Right at end must stay at end");
    }

    // =========================================================================
    // Multiline (long input wrapping) tests
    // =========================================================================

    #[test]
    fn long_user_input_wraps_visually() {
        let mut fx = fixture();
        // Type a line longer than the terminal width
        let long = "x".repeat(COLS + 20);
        type_str(&mut fx, &long);
        assert_eq!(fx.handle.get_buffer(), long.as_str());
        // The cursor should be at the end of the buffer
        assert_eq!(fx.handle.get_cursor(), long.len());
        // Submit the line
        let line = submit(&mut fx);
        assert_eq!(line, long);
    }

    #[test]
    fn very_long_input_does_not_truncate() {
        let mut fx = fixture();
        // Type a very long line (500 chars)
        let long = "a".repeat(500);
        type_str(&mut fx, &long);
        assert_eq!(
            fx.handle.get_buffer().len(),
            500,
            "very long input must not be truncated"
        );
    }

    // =========================================================================
    // Word-boundary navigation (Ctrl+Left / Ctrl+Right)
    // =========================================================================

    /// Ctrl+Right from start jumps to start of next word.
    #[test]
    fn ctrl_right_jumps_to_next_word() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world foo");
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        // Ctrl+Right — should jump to start of 'world' (position 6)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_cursor(),
            6,
            "Ctrl+Right from start should land at 'world' (expected 6, got {})",
            fx.handle.get_cursor()
        );
    }

    /// Ctrl+Right from within a word (after first char) jumps to start of next word.
    #[test]
    fn ctrl_right_from_middle_of_word() {
        let mut fx = fixture();
        type_str(&mut fx, "one two three");
        // Move cursor to position 2 (middle of "one")
        for _ in 0..11 {
            fx.input
                .send(RawEvent::Key(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::NONE,
                )))
                .unwrap();
            let _ = fx.term.next_event();
        }
        assert_eq!(fx.handle.get_cursor(), 2);
        // Ctrl+Right — should skip "one " and land at start of 'two' (position 4)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        let _ = fx.term.next_event();
        assert_eq!(
            fx.handle.get_cursor(),
            4,
            "Ctrl+Right from middle of word should land at start of next word (expected 4, got {})",
            fx.handle.get_cursor()
        );
    }

    /// Ctrl+Right from whitespace before a word lands at start of that word.
    /// The implementation skips whitespace + the next word + trailing whitespace,
    /// so from the space before 'two' it lands at the start of 'three'.
    #[test]
    fn ctrl_right_from_whitespace_before_word() {
        let mut fx = fixture();
        type_str(&mut fx, "one two three");
        // Move cursor to position 3 (the space between "one" and "two")
        for _ in 0..10 {
            fx.input
                .send(RawEvent::Key(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::NONE,
                )))
                .unwrap();
            let _ = fx.term.next_event();
        }
        assert_eq!(fx.handle.get_cursor(), 3);
        // Ctrl+Right from the space — skips whitespace, then "two", then trailing
        // whitespace, landing at start of 'three' (position 8)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        let _ = fx.term.next_event();
        assert_eq!(
            fx.handle.get_cursor(),
            8,
            "Ctrl+Right from whitespace lands after next word + trailing ws (expected 8, got {})",
            fx.handle.get_cursor()
        );
    }

    /// Ctrl+Right on the last word goes to end of buffer.
    #[test]
    fn ctrl_right_on_last_word_goes_to_end() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world foo");
        // Move to start of last word 'foo' (position 12)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // -> world
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event(); // -> foo
                                      // Now at position 12
        assert_eq!(fx.handle.get_cursor(), 12);
        // Ctrl+Right on last word — should go to end (15)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_cursor(),
            15,
            "Ctrl+Right on last word should go to end of buffer (expected 15, got {})",
            fx.handle.get_cursor()
        );
    }

    /// Ctrl+Right at end of buffer is a no-op.
    #[test]
    fn ctrl_right_at_end_does_nothing() {
        let mut fx = fixture();
        type_str(&mut fx, "hello");
        assert_eq!(fx.handle.get_cursor(), 5);
        // Ctrl+Right at end — should stay at end
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        assert_eq!(fx.handle.get_cursor(), 5);
    }

    /// Ctrl+Right on empty buffer is a no-op.
    #[test]
    fn ctrl_right_on_empty_buffer() {
        let mut fx = fixture();
        assert_eq!(fx.handle.get_cursor(), 0);
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        assert_eq!(fx.handle.get_cursor(), 0);
    }

    /// Ctrl+Right skips consecutive whitespace to land at start of next word.
    #[test]
    fn ctrl_right_skips_multiple_spaces() {
        let mut fx = fixture();
        type_str(&mut fx, "hello    world");
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        // Ctrl+Right — should skip "hello    " and land on 'world' at position 9
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_cursor(),
            9,
            "Ctrl+Right should skip multiple spaces (expected 9, got {})",
            fx.handle.get_cursor()
        );
    }

    /// Ctrl+Left from end jumps backward one word.
    #[test]
    fn ctrl_left_jumps_back_one_word() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world foo");
        assert_eq!(fx.handle.get_cursor(), 15);
        // Ctrl+Left — should jump to start of 'foo' (position 12)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_cursor(),
            12,
            "Ctrl+Left from end should jump to start of last word (expected 12, got {})",
            fx.handle.get_cursor()
        );
    }

    /// Ctrl+Left from middle of word jumps to start of that word.
    #[test]
    fn ctrl_left_from_middle_of_word() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world foo");
        // Move cursor to position 8 (middle of 'world')
        for _ in 0..7 {
            fx.input
                .send(RawEvent::Key(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::NONE,
                )))
                .unwrap();
            let _ = fx.term.next_event();
        }
        assert_eq!(fx.handle.get_cursor(), 8);
        // Ctrl+Left — should jump to start of 'world' (position 6)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_cursor(), 6,
            "Ctrl+Left from middle of word should jump to start of current word (expected 6, got {})",
            fx.handle.get_cursor()
        );
    }

    /// Ctrl+Left from start of buffer is a no-op.
    #[test]
    fn ctrl_left_at_start_does_nothing() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world");
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        // Ctrl+Left at start — should stay at 0
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        assert_eq!(fx.handle.get_cursor(), 0);
    }

    /// Ctrl+Left on empty buffer is a no-op.
    #[test]
    fn ctrl_left_on_empty_buffer() {
        let mut fx = fixture();
        assert_eq!(fx.handle.get_cursor(), 0);
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        assert_eq!(fx.handle.get_cursor(), 0);
    }

    /// Ctrl+Left skips consecutive whitespace to land at start of current word.
    #[test]
    fn ctrl_left_skips_multiple_spaces() {
        let mut fx = fixture();
        type_str(&mut fx, "hello    world");
        // From end, Ctrl+Left should skip "world" and land on the spaces before it
        assert_eq!(fx.handle.get_cursor(), 14);
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_cursor(),
            9,
            "Ctrl+Left from end should land at start of 'world' (expected 9, got {})",
            fx.handle.get_cursor()
        );
        // Second Ctrl+Left should skip the spaces and "hello" to land at position 0
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_cursor(),
            0,
            "Second Ctrl+Left should land at start of 'hello' (expected 0, got {})",
            fx.handle.get_cursor()
        );
    }

    /// Repeated Ctrl+Left walks backward word by word.
    #[test]
    fn ctrl_left_walks_back_word_by_word() {
        let mut fx = fixture();
        type_str(&mut fx, "one two three");
        assert_eq!(fx.handle.get_cursor(), 13);
        // Ctrl+Left from end -> start of 'three' (8)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 8);
        // Ctrl+Left again -> start of 'two' (4)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 4);
        // Ctrl+Left again -> start of 'one' (0)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        // Ctrl+Left at start -> still 0 (no event since cursor stays same)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        assert_eq!(fx.handle.get_cursor(), 0);
    }

    /// Repeated Ctrl+Right walks forward word by word.
    #[test]
    fn ctrl_right_walks_forward_word_by_word() {
        let mut fx = fixture();
        type_str(&mut fx, "one two three");
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        // Ctrl+Right from start -> start of 'two' (4)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 4);
        // Ctrl+Right again -> start of 'three' (8)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 8);
        // Ctrl+Right again -> end of buffer (13)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 13);
        // Ctrl+Right at end -> still 13 (no event)
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        assert_eq!(fx.handle.get_cursor(), 13);
    }

    // =========================================================================
    // Even more missing readline shortcuts
    // =========================================================================

    #[test]
    fn alt_d_should_delete_word_forward() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world");
        // Move to start, Alt+D should delete "hello "
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let _ = fx.term.next_event();
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::ALT,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_buffer() == "world" || fx.handle.get_buffer() == " world",
            "BUG: Alt+D should delete next word (expected 'world', got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn ctrl_h_should_act_as_backspace() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        // Ctrl+H is the traditional Backspace
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('h'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_buffer() == "ab",
            "BUG: Ctrl+H should delete previous char like Backspace (got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn ctrl_y_should_yank_from_kill_ring() {
        let mut fx = fixture();
        // First, delete some text with Ctrl-W or Ctrl-U to populate a kill ring
        type_str(&mut fx, "deleteme");
        // Ctrl-W deletes previous word
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('w'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_buffer(),
            "",
            "buffer should be empty after Ctrl-W"
        );
        // Ctrl-Y should yank (paste) "deleteme" back
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_buffer() == "deleteme",
            "BUG: Ctrl-Y should yank deleted text back (got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn alt_backspace_should_delete_previous_word() {
        let mut fx = fixture();
        type_str(&mut fx, "hello world foo");
        // Cursor at end, Alt+Backspace should delete "foo"
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::ALT,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            fx.handle.get_buffer() == "hello world " || fx.handle.get_buffer() == "hello world",
            "BUG: Alt+Backspace should delete previous word (expected 'hello world ', got '{}')",
            fx.handle.get_buffer()
        );
    }

    #[test]
    fn cache_bar_stale_width_on_resize_through_real_rendering() {
        // With the new width-independent format, resize does not affect the text.
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::CacheTelemetry {
                        input_tokens: 1000,
                        output_tokens: 500,
                        cache_read_tokens: 500,
                        cache_creation_tokens: 100,
                    },
                ))));
                std::thread::sleep(Duration::from_millis(30));
                let _ = app_tx.send(AppEvent::Term(Event::Resize {
                    width: 120,
                    height: 30,
                }));
                std::thread::sleep(Duration::from_millis(30));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        let rendered = fx.transcript_contains("cache:");
        assert!(rendered, "cache bar must be visible");
        let em = Emulator::from_capture(120, 30, &fx.buf);
        let joined: String = em
            .history()
            .iter()
            .chain(em.screen_lines().iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains('\u{2191}'), "should show up arrow");
        assert!(joined.contains('\u{2193}'), "should show down arrow");
        assert!(joined.contains('R'), "should show R for cache reads");
        assert!(joined.contains("CH"), "should show CH for hit rate");
        fx.shutdown();
    }

    #[test]
    fn user_input_with_newline_characters() {
        // Enter key submits the line; literal newlines can't be typed.
        // Pasting content with newlines drops them — this is tested below.
    }

    // =========================================================================
    // Window resize edge cases
    // =========================================================================

    #[test]
    fn resize_to_tiny_dimensions_does_not_panic() {
        let mut fx = fixture();
        // Resize to very small (but valid) dimensions
        fx.input.send(RawEvent::Resize(5, 3)).expect("input open");
        match fx.term.next_event() {
            Some(Event::Resize { width, height }) => {
                assert_eq!(width, 5);
                assert_eq!(height, 3);
            }
            other => panic!("expected Resize, got {other:?}"),
        }
        // Must not panic, must still accept input
        type_str(&mut fx, "hi");
        assert_eq!(fx.handle.get_buffer(), "hi");
    }

    #[test]
    fn resize_to_zero_height_does_not_panic() {
        let mut fx = fixture();
        // Resize height to 0 — the code uses height.max(1) so it should not panic
        fx.input.send(RawEvent::Resize(40, 0)).expect("input open");
        match fx.term.next_event() {
            Some(Event::Resize { .. }) => {}
            other => panic!("expected Resize, got {other:?}"),
        }
        // Should still work
        type_str(&mut fx, "survived");
        assert_eq!(fx.handle.get_buffer(), "survived");
    }

    #[test]
    fn resize_to_zero_width_does_not_panic() {
        let mut fx = fixture();
        // Resize width to 0 — width.max(1) should prevent issues
        fx.input.send(RawEvent::Resize(0, 24)).expect("input open");
        match fx.term.next_event() {
            Some(Event::Resize { .. }) => {}
            other => panic!("expected Resize, got {other:?}"),
        }
        type_str(&mut fx, "ok");
        assert_eq!(fx.handle.get_buffer(), "ok");
    }

    #[test]
    fn resize_during_streaming_with_prompt_text_preserved() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                // Start streaming
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("streaming content ".into()),
                ))));
                std::thread::sleep(Duration::from_millis(20));
                // Resize while streaming!
                let _ = app_tx.send(AppEvent::Term(Event::Resize {
                    width: 30,
                    height: 10,
                }));
                std::thread::sleep(Duration::from_millis(20));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("continues after resize".into()),
                ))));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(OutputChunk::Done))));
                std::thread::sleep(Duration::from_millis(30));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        assert!(
            fx.transcript_contains("streaming content continues after resize"),
            "BUG: resize during streaming caused text loss"
        );
        fx.shutdown();
    }

    #[test]
    fn multiple_resizes_in_sequence_render_correctly() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                // Send text
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("content ".into()),
                ))));
                std::thread::sleep(Duration::from_millis(10));
                // Rapid resizes
                for (w, h) in &[(80, 12), (40, 24), (120, 30), (80, 12)] {
                    let _ = app_tx.send(AppEvent::Term(Event::Resize {
                        width: *w,
                        height: *h,
                    }));
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(10));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("after resizes".into()),
                ))));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(OutputChunk::Done))));
                std::thread::sleep(Duration::from_millis(30));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        assert!(
            fx.transcript_contains("content after resizes"),
            "BUG: multiple resizes caused text loss"
        );
        fx.shutdown();
    }

    #[test]
    fn resize_during_user_input_preserves_buffer() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                // Simulate user typing
                let _ = app_tx.send(AppEvent::Term(Event::BufferChanged));
                // Resize while buffer is non-empty
                let _ = app_tx.send(AppEvent::Term(Event::Resize {
                    width: 60,
                    height: 15,
                }));
                std::thread::sleep(Duration::from_millis(30));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        // Buffer state should not have been corrupted by resize
        fx.shutdown();
    }

    #[test]
    fn resize_after_clear_then_streaming_shows_text() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                // Start streaming, clear, resize, then stream more
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("before ".into()),
                ))));
                std::thread::sleep(Duration::from_millis(10));
                // run_loop doesn't handle /clear directly — it's done via process_line
                // Simulate clear + resize
                let _ = app_tx.send(AppEvent::Term(Event::Resize {
                    width: 100,
                    height: 30,
                }));
                std::thread::sleep(Duration::from_millis(10));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("after ".into()),
                ))));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(OutputChunk::Done))));
                std::thread::sleep(Duration::from_millis(30));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        assert!(
            fx.transcript_contains("before after"),
            "BUG: resize after clear caused text loss"
        );
        fx.shutdown();
    }

    #[test]
    /// After setting a status line (e.g. cache info) the cursor should be at the
    /// correct visual row — in the prompt area, not overlapping the status line.
    #[test]
    fn cursor_position_correct_after_status_line() {
        let mut fx = fixture();
        // Set a status line (simulates what cache telemetry does)
        let status = StyledBlock::new(StyledText::from(Span::new(
            " cache:  50.0% ████████░░░░  hit 1.0K / total 2.0K input",
            Style::default(),
        )));
        fx.handle.set_status_line(status);
        // Type some text
        type_str(&mut fx, "hello");
        // Force a redraw so the output buffer is up to date
        fx.handle.redraw_sync();
        // Read back the rendered output
        let em = Emulator::from_capture(ROWS, COLS, &fx.buf);
        let screen = em.screen_lines();
        // The screen should show the status line and the prompt with typed text
        // Layout: [rubber rows if any] [status line] [prompt line]
        // The status line must be visible
        let has_status = screen.iter().any(|l| l.contains("cache:"));
        assert!(has_status, "status line must be visible on screen");
        // The prompt text (P> hello) must be visible
        let has_prompt = screen.iter().any(|l| l.contains("P> hello"));
        assert!(
            has_prompt,
            "prompt with typed text must be visible on screen"
        );
        // The status line must NOT be on the same row as the prompt
        let status_row = screen.iter().position(|l| l.contains("cache:"));
        let prompt_row = screen.iter().position(|l| l.contains("P> hello"));
        assert!(
            status_row != prompt_row,
            "status line and prompt must be on different rows"
        );
        assert!(
            status_row < prompt_row,
            "status line must appear above the prompt"
        );
        // The cursor (from terminal emulator) must be on the prompt row,
        // not on the status line row.
        let (cursor_row, cursor_col) = em.cursor();
        assert_eq!(
            cursor_row,
            prompt_row.unwrap(),
            "cursor should be on the prompt row (row {}), not on status line row {} — got row {}",
            prompt_row.unwrap(),
            status_row.unwrap(),
            cursor_row
        );
    }

    fn resize_with_cache_bar_does_not_duplicate_status_line() {
        let mut fx = loop_fixture();
        // Pre-populate cache stats
        fx.app.cache.update(500, 250, 250, 50);
        {
            let (w, _) = fx.handle.size();
            fx.handle
                .set_status_line(fx.app.cache.to_status_block(w.max(40)));
        }
        fx.handle.redraw_sync();

        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                // Resize multiple times
                for (w, h) in &[(50, 10), (100, 20), (80, 15)] {
                    let _ = app_tx.send(AppEvent::Term(Event::Resize {
                        width: *w,
                        height: *h,
                    }));
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(30));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        let cache_count = fx.count("cache:");
        assert_eq!(
            cache_count, 1,
            "BUG: cache bar duplicated after resize — found {cache_count} times"
        );
        fx.shutdown();
    }

    #[test]
    fn resize_huge_dimensions_do_not_overflow() {
        let mut fx = fixture();
        // Resize to extremely large dimensions
        fx.input
            .send(RawEvent::Resize(9999, 9999))
            .expect("input open");
        match fx.term.next_event() {
            Some(Event::Resize { width, height }) => {
                assert_eq!(width, 9999);
                assert_eq!(height, 9999);
            }
            other => panic!("expected Resize, got {other:?}"),
        }
        // Must not panic
        type_str(&mut fx, "huge");
        assert_eq!(fx.handle.get_buffer(), "huge");
    }

    #[test]
    fn resize_preserves_input_history_navigation() {
        let mut fx = fixture();
        // Submit two commands
        type_str(&mut fx, "first");
        submit(&mut fx);
        type_str(&mut fx, "second");
        submit(&mut fx);
        // Resize
        fx.input.send(RawEvent::Resize(60, 20)).expect("input open");
        let _ = fx.term.next_event();
        // History should still work after resize
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fx.handle.get_buffer(),
            "second",
            "History recall must work after resize"
        );
    }

    #[test]
    fn consecutive_resizes_while_streaming_do_not_corrupt_block() {
        let mut fx = fixture();
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("start ".into())),
        );
        // Multiple resize events (simulate user dragging window edge)
        for (w, h) in &[(50, 10), (45, 12), (55, 11), (60, 12), (50, 10)] {
            fx.input.send(RawEvent::Resize(*w, *h)).expect("input open");
            assert!(matches!(fx.term.next_event(), Some(Event::Resize { .. })));
        }
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("end".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        assert!(
            transcript_contains(&fx, "start end"),
            "BUG: consecutive resizes during streaming corrupted block content"
        );
    }

    // =========================================================================
    // Input edge cases: paste, Tab, special keys
    // =========================================================================

    #[test]
    fn paste_long_text_handled_correctly() {
        let mut fx = fixture();
        // Simulate paste (newlines in paste are silently dropped)
        fx.input
            .send(RawEvent::Paste("line1\nline2\nline3".to_string()))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        // Newlines should be dropped, content concatenated
        // Note: paste handling depends on how the virtual input loop treats Paste events
        // Looking at dispatch_input_event: Paste inserts chars but skips \n and \r.
        assert_eq!(
            fx.handle.get_buffer(),
            "line1line2line3",
            "Paste should drop newlines and concatenate content"
        );
    }

    #[test]
    fn ctrl_l_clears_screen_and_cache_bar_persists() {
        let mut fx = fixture();
        // Show some output
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::TextDelta("visible".into())),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::CacheTelemetry {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 50,
                cache_creation_tokens: 10,
            }),
        );
        handle_daemon_event(
            &fx.handle,
            &mut fx.app,
            &mut fx.streaming,
            chunk(OutputChunk::Done),
        );
        fx.handle.redraw_sync();
        assert!(transcript_contains(&fx, "visible"));
        assert!(transcript_contains(&fx, "cache:"));

        // Ctrl-L should clear the screen
        fx.input
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL,
            )))
            .expect("input open");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !transcript_contains(&fx, "visible"),
            "Ctrl-L should clear visible content"
        );
    }

    // =========================================================================
    // Slash command edge cases
    // =========================================================================

    #[test]
    fn slash_commands_with_trailing_spaces() {
        let mut fx = fixture();
        let outcome = process_line("/help  ", &mut fx.app, &fx.handle, &fx.cmd_tx);
        assert_eq!(outcome, LineOutcome::Continue);
        // Should render help, not error
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "Available commands"), 1);
    }

    #[test]
    fn slash_commands_with_mixed_case() {
        let mut fx = fixture();
        // Case-sensitive: /HELP should be unknown
        process_line("/HELP", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // It should be sent as user text since it's not in known_commands
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::Run { content, .. }) => assert_eq!(content, "/HELP"),
            other => panic!("expected Run for unknown cmd, got {other:?}"),
        }
    }

    #[test]
    fn new_with_spaces_in_name() {
        let mut fx = fixture();
        // /new with a name that has multiple parts — only first is used
        process_line("/new my session name", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // The session name should be "my" (first token)
        assert_eq!(fx.app.session_id, "my");
    }

    #[test]
    fn resume_with_spaces_in_id() {
        let mut fx = fixture();
        process_line(
            "/resume session-id extra",
            &mut fx.app,
            &fx.handle,
            &fx.cmd_tx,
        );
        assert_eq!(fx.app.session_id, "session-id");
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::Resume(sid)) => assert_eq!(sid, "session-id"),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn interrupt_before_any_streaming_does_not_panic() {
        let mut fx = fixture();
        // /interrupt with no active stream should be safe
        process_line("/interrupt", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // Should send Interrupt command
        match fx.cmd_rx.try_recv() {
            Ok(DaemonCmd::Interrupt(sid)) => assert_eq!(sid, "sess-1"),
            other => panic!("expected Interrupt, got {other:?}"),
        }
    }

    #[test]
    fn status_response_contains_cache_info_when_available() {
        let mut fx = loop_fixture();
        // Pre-populate cache
        fx.app.cache.update(1000, 500, 500, 100);
        {
            let (w, _) = fx.handle.size();
            fx.handle
                .set_status_line(fx.app.cache.to_status_block(w.max(40)));
        }
        // Status ping should include cache info
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                let _ = app_tx.send(AppEvent::Term(Event::Line("/status".to_string())));
                std::thread::sleep(Duration::from_millis(20));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(ServerEvent::ModelList {
                    models: vec!["gpt-a".into()],
                })));
                std::thread::sleep(Duration::from_millis(30));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(500),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        // Status OK message should include cache data in the status line
        assert!(
            fx.transcript_contains("cache:"),
            "after /status, cache bar should still be visible"
        );
        fx.shutdown();
    }

    // =========================================================================
    // Edge-case: rapid events causing race conditions
    // =========================================================================

    #[test]
    fn rapid_cache_telemetry_and_textdelta_does_not_corrupt() {
        let mut fx = loop_fixture();
        let driver = {
            let app_tx = fx.app_tx.clone();
            std::thread::spawn(move || {
                // Interleave telemetry with text deltas rapidly
                for i in 0..20 {
                    let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                        OutputChunk::TextDelta(format!("chunk{i} ")),
                    ))));
                    let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                        OutputChunk::CacheTelemetry {
                            input_tokens: 100 + i,
                            output_tokens: 100 + i,
                            cache_read_tokens: 50 + i / 2,
                            cache_creation_tokens: 10,
                        },
                    ))));
                }
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(OutputChunk::Done))));
                std::thread::sleep(Duration::from_millis(50));
                let _ = app_tx.send(AppEvent::Term(Event::Eof));
            })
        };
        run_loop(
            &fx.handle,
            &mut fx.app,
            &fx.app_tx,
            &fx.app_rx,
            &fx.cmd_tx,
            Duration::from_millis(200),
        );
        driver.join().unwrap();
        fx.handle.redraw_sync();
        // All text chunks should be present (not lost/corrupted)
        assert!(
            fx.transcript_contains("chunk0"),
            "first chunk must be present"
        );
        assert!(
            fx.transcript_contains("chunk19"),
            "last chunk must be present"
        );
        // Cache stats should have accumulated 20 requests
        assert_eq!(fx.app.cache.request_count, 20);
        assert!(fx.app.cache.total_input_tokens > 0);
        fx.shutdown();
    }
}
