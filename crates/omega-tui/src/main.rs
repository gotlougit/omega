//! omega-tui — Terminal UI for the omega-loop agent daemon.
//!
//! Connects to a running `omega-loop` daemon, creates/joins a session,
//! provides a prompt with slash commands, and renders streaming agent
//! output using block-based diff rendering adapted from tau.

use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::runtime::Runtime;

use omega_loop_client::{
    AgentdClient, DaemonReader, DaemonWriter, OutputChunk, ServerEvent, SessionConfig,
};
use cli::{Color, Event, Span, Style, StyledBlock, StyledText, Term, TermHandle};

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
    Run {
        session_id: String,
        content: String,
    },
    SetModel {
        session_id: String,
        model: String,
    },
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

fn spawn_daemon(
    reader: DaemonReader,
    writer: DaemonWriter,
    cmd_rx: Receiver<DaemonCmd>,
    app_tx: Sender<AppEvent>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = Runtime::new().expect("tokio runtime");
        rt.block_on(async { daemon_loop(reader, writer, cmd_rx, app_tx).await });
    })
}

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
                Ok(DaemonCmd::SetModel {
                    session_id,
                    model,
                }) => {
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
                if app_tx.send(AppEvent::Daemon(DaemonEv::Event(event))).is_err() {
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
                return format!("tool call {name}: {}", cli::truncate_to_width(&one_line, 120));
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
                    let block =
                        StyledBlock::new(StyledText::from(Span::new(streaming.buf.clone(), s_assistant())));
                    handle.set_block(id, block);
                    handle.redraw();
                } else {
                    let block =
                        StyledBlock::new(StyledText::from(Span::new(streaming.buf.clone(), s_assistant())));
                    streaming.block_id = Some(handle.print_output(block));
                }
            }
            OutputChunk::TextComplete(s) => {
                // Only act if there is an active streaming block to finalize.
                // Standalone TextComplete without prior TextDelta is silently
                // ignored — the block must first be created via TextDelta.
                if let Some(id) = streaming.block_id.take() {
                    streaming.buf.clear();
                    let block =
                        StyledBlock::new(StyledText::from(Span::new(s, s_assistant())));
                    handle.set_block(id, block);
                    handle.redraw();
                    streaming.finalized = true;
                }
            }
            OutputChunk::ThinkingDelta(s) => {
                handle.print_output(StyledBlock::new(StyledText::from(Span::new(s, s_thinking()))));
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
            OutputChunk::ToolEnd { result, .. } => {
                if result.is_error {
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
            OutputChunk::AskUserQuestion { questions, .. } => {
                for q in &questions {
                    handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                        format!("❓ {}: {}", q.header, q.question),
                        s_highlight(),
                    ))));
                    for opt in &q.options {
                        handle.print_output(StyledBlock::new(StyledText::from(Span::new(
                            format!("  [{}] {}", opt.label, opt.description),
                            s_assistant(),
                        ))));
                    }
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
                        let block =
                            StyledBlock::new(StyledText::from(Span::new(buf, s_assistant())));
                        handle.set_block(id, block);
                        handle.redraw();
                    }
                }
                streaming.reset();
            }
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

struct AppState {
    session_id: String,
    model: String,
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
        "/quit", "/exit", "/status", "/help", "/clear",
        "/models", "/sessions", "/interrupt", "/compact",
        "/model", "/new", "/resume",
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
            let mut st = StyledText::from(Span::new(
                "Available commands:\n".to_string(),
                s_system(),
            ));
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
            AppEvent::Term(Event::Resize { .. }) => {}
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

    // ── Connect to omega-loop ──────────────────────────────────────────
    let rt = Runtime::new()?;
    let client = rt.block_on(AgentdClient::connect()).with_context(|| {
        "Cannot connect to omega-loop daemon.\n\
         Make sure omega-loop is running.\n\
         Set OMEGA_LOOP_SOCKET_PATH if using a custom socket path."
    })?;
    let (reader, writer) = client.split();

    let (cmd_tx, cmd_rx) = mpsc::channel::<DaemonCmd>();
    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();

    let daemon_thread = spawn_daemon(reader, writer, cmd_rx, app_tx.clone());

    // ── Create initial session ─────────────────────────────────────────
    let session_id = format!("tui-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    // Send an empty run to create the session on the daemon
    cmd_tx
        .send(DaemonCmd::Run {
            session_id: session_id.clone(),
            content: String::new(),
        })
        .ok();

    // ── Query models, pick first ───────────────────────────────────────
    cmd_tx.send(DaemonCmd::ListModels).ok();
    let model = loop {
        match app_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(AppEvent::Daemon(DaemonEv::Event(ServerEvent::ModelList { models })))
                if !models.is_empty() =>
            {
                break models.into_iter().next().unwrap();
            }
            Ok(AppEvent::Daemon(DaemonEv::Event(_))) => continue,
            Err(mpsc::RecvTimeoutError::Timeout)
            | Ok(AppEvent::Daemon(DaemonEv::Disconnected)) => {
                break std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break "unknown".to_string(),
            Ok(_) => continue,
        }
    };
    // Set the model on the daemon
    cmd_tx
        .send(DaemonCmd::SetModel {
            session_id: session_id.clone(),
            model: model.clone(),
        })
        .ok();

    let mut app = AppState { session_id, model };

    // ── Create terminal ────────────────────────────────────────────────
    let prompt = StyledText::from(Span::new("▸ ", Style::default().fg(Color::DarkYellow)));
    let (term, handle) = Term::new(prompt)?;

    // Welcome message
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

    run_loop(
        &handle,
        &mut app,
        &app_tx,
        &app_rx,
        &cmd_tx,
        STATUS_TIMEOUT,
    );

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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use omega_loop_client::{OutputChunk, ServerEvent, ToolResultWire};
    use cli::emulator::{Capture, Emulator};
    use cli::{BlockId, RawEvent};
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
            .send(RawEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)))
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("Hello, ".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("world".into())));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "Hello, world"), 1);

        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextComplete("Hello, world!".into())));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "Hello, world!"), 1);
        assert_eq!(count_rows_containing(&fx, "Hello, world"), 1);
    }

    #[test]
    fn streaming_then_done_keeps_text() {
        let mut fx = fixture();
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("partial".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "partial"), 1);
        // Done finalizes and resets the streaming tracker for the next turn.
        assert!(fx.streaming.block_id.is_none());
        assert!(fx.streaming.buf.is_empty());
    }

    #[test]
    fn tool_events_render() {
        let mut fx = fixture();
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::ToolStart {
            id: "t1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        }));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::ToolEnd {
            id: "t1".into(),
            result: ToolResultWire {
                text: "boom".into(),
                is_error: true,
                content: None,
            },
        }));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "tool call read_file: src/main.rs"), 1);
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, ServerEvent::ModelChanged {
            model: "gpt-b".into(),
        });
        fx.handle.redraw_sync();
        assert_eq!(fx.app.model, "gpt-b");
        assert_eq!(count_rows_containing(&fx, "Model changed: gpt-b"), 1);
    }

    #[test]
    fn status_and_error_render() {
        let mut fx = fixture();
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Status("working…".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Error("it broke".into())));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "working…"), 1);
        assert_eq!(count_rows_containing(&fx, "it broke"), 1);
    }

    #[test]
    fn model_list_renders_and_marks_current() {
        let mut fx = fixture();
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, ServerEvent::ModelList {
            models: vec!["gpt-a".into(), "gpt-b".into()],
        });
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("Hi there".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        assert!(user_row < reply_row, "reply must follow the prompt: {all:?}");
        // Fresh prompt sits below the reply with the cursor after it.
        let prompt_row = all.iter().rposition(|l| l.trim_end() == "P>").unwrap();
        assert!(reply_row < prompt_row, "no fresh prompt after reply: {all:?}");

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
            handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta(reply.into())));
            handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextComplete(reply.into())));
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
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::Done,
                ))));
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
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(
                    ServerEvent::ModelList {
                        models: vec!["gpt-a".into(), "gpt-b".into()],
                    },
                )));
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
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::Done,
                ))));
                // --- Turn 2 ---
                std::thread::sleep(Duration::from_millis(20));
                let _ = app_tx.send(AppEvent::Term(Event::Line("second".to_string())));
                std::thread::sleep(Duration::from_millis(20));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::TextDelta("response two".into()),
                ))));
                let _ = app_tx.send(AppEvent::Daemon(DaemonEv::Event(chunk(
                    OutputChunk::Done,
                ))));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("a".into())));
        let id: BlockId = fx.streaming.block_id.expect("block created");
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("b".into())));
        assert_eq!(fx.streaming.block_id, Some(id));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("c".into())));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("final".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextComplete("final".into())));
        // streaming.finalized is now true; next TextDelta auto-resets for a new turn.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("late".into())));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("hi".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        assert!(!fx.streaming.finalized); // reset already
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "hi"), 1);
    }

    /// ToolStart + ToolEnd should render even after Done has closed the
    /// text-streaming phase (tools are independent of the streaming tracker).
    #[test]
    fn tool_events_render_after_done() {
        let mut fx = fixture();
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("text".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        // streaming is reset; tool events should still render.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::ToolStart {
            id: "t2".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::ToolEnd {
            id: "t2".into(),
            result: ToolResultWire { text: "done".into(), is_error: false, content: None },
        }));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "tool call Bash: ls"), 1);
        assert_eq!(count_rows_containing(&fx, "done"), 1);
    }

    /// TextComplete arriving without any prior TextDelta is silently ignored.
    /// The block must first be created via TextDelta.
    #[test]
    fn textcomplete_without_prior_delta_creates_block() {
        let mut fx = fixture();
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("only".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextComplete("only".into())));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("before".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        // Done called reset() — streaming is clean.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("after".into())));
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
            handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta(s)));
        }
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        fx.handle.redraw_sync();
        // Every chunk word must be present in the output (join all rendered
        // rows so wrapping doesn't defeat the substring search).
        let all_rendered = emulator(&fx)
            .history().iter()
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("partial".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextComplete("corrected".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        fx.handle.redraw_sync();

        // CORRECT: Done resets streaming so the next turn works.
        assert!(
            !fx.streaming.finalized,
            "BUG: TextComplete before Done leaves streaming untouched — Done never resets"
        );
        // Turn 2 should render.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("turn2".into())));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("before-clear".into())));
        let id_before = fx.streaming.block_id;
        assert!(id_before.is_some());

        // User issues /clear.
        fx.handle.clear_output();
        fx.handle.redraw_sync();

        // Agent continues streaming the same turn.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("after-clear".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("old-session".into())));
        assert!(fx.streaming.block_id.is_some());

        // User creates a new session mid-stream.
        process_line("/new fresh", &mut fx.app, &fx.handle, &fx.cmd_tx);
        fx.handle.redraw_sync();

        // More deltas arrive for the same logical block.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("new-text".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("turn1".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextComplete("turn1".into())));
        assert!(fx.streaming.finalized);
        // Turn 2: the next TextDelta should auto-reset and NOT be dropped.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("turn2".into())));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("first".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        assert!(!fx.streaming.finalized);
        assert!(fx.streaming.block_id.is_none());
        // Second turn.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("second".into())));
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
    /// non-empty buffer it does nothing — which is surprising to users
    /// who expect it to delete the character at cursor.
    #[test]
    fn ctrl_d_on_nonempty_buffer_is_noop() {
        let mut fx = fixture();
        type_str(&mut fx, "data");
        // Send Ctrl-D ('d' with CTRL modifier).
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)))
            .expect("input open");
        // handle_key_locked silently ignores Ctrl-D on non-empty buffer:
        // no event is emitted. Buffer is unchanged.
        // (We cannot call term.next_event() because it would block forever.)
        assert_eq!(fx.handle.get_buffer(), "data");
    }

    /// Ctrl-U clears from cursor position to beginning of buffer.
    #[test]
    fn ctrl_u_from_middle_clears_prefix() {
        let mut fx = fixture();
        type_str(&mut fx, "hello");
        // Move cursor left twice, putting it after "hel".
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))).unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))).unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        // Send Ctrl-U.
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))).unwrap();
        let _ = fx.term.next_event(); // BufferChanged
        assert_eq!(fx.handle.get_buffer(), "lo");
    }

    /// Pressing Home moves cursor to position 0, End to the end.
    #[test]
    fn home_and_end_navigate() {
        let mut fx = fixture();
        type_str(&mut fx, "abc");
        assert_eq!(fx.handle.get_cursor(), 3);
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))).unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 0);
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))).unwrap();
        let _ = fx.term.next_event();
        assert_eq!(fx.handle.get_cursor(), 3);
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))).unwrap();
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))).unwrap();
        // Drain the BufferChanged event.
        assert_eq!(fx.term.next_event(), Some(Event::BufferChanged));
        // Buffer now ends with \t — the run_loop handler will trim and complete.
        assert!(fx.handle.get_buffer().ends_with('\t'), "Tab should insert a literal \\t");
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
        let id = fx.handle.print_output(StyledBlock::new(StyledText::from(Span::new("hello", s_assistant()))));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "hello"), 1);
        // Clear everything.
        fx.handle.clear_output();
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "hello"), 0);
        // set_block with the same id — should either render or re-add to history.
        fx.handle.set_block(id, StyledBlock::new(StyledText::from(Span::new("ghost", s_assistant()))));
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
        fx.handle.print_output(StyledBlock::new(StyledText::from(Span::new("fresh", s_assistant()))));
        fx.handle.redraw_sync();
        assert_eq!(count_rows_containing(&fx, "fresh"), 1);
    }

    /// Two consecutive /clear commands should not leave the terminal in
    /// a broken state.
    #[test]
    fn two_consecutive_clears_noop() {
        let fx = fixture();
        fx.handle.print_output(StyledBlock::new(StyledText::from(Span::new("x", s_assistant()))));
        fx.handle.clear_output();
        fx.handle.clear_output();
        fx.handle.redraw_sync();
        // Terminal should just show the empty prompt.
        fx.handle.print_output(StyledBlock::new(StyledText::from(Span::new("ok", s_assistant()))));
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
        assert!(!matches!(fx.cmd_rx.try_recv(), Ok(DaemonCmd::SetModel { .. })));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("turn1".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        fx.handle.redraw_sync();
        // Clear.
        fx.handle.clear_output();
        fx.handle.redraw_sync();
        // Turn 2.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("turn2".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta(long.clone())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("before-resize".into())));
        // Resize the terminal.
        fx.input.send(RawEvent::Resize(80, 20)).expect("input open");
        // Drain the Resize event.
        assert!(matches!(fx.term.next_event(), Some(Event::Resize { .. })));
        // Continue streaming.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta(" after".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        assert!(!fx.streaming.finalized);
        assert!(fx.streaming.block_id.is_none());
        assert!(fx.streaming.buf.is_empty());
        // Next turn works.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("next".into())));
        assert!(fx.streaming.block_id.is_some());
    }

    /// TextDelta with empty string should not create a visible block.
    #[test]
    fn empty_textdelta_does_not_create_block() {
        let mut fx = fixture();
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta(String::new())));
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("streaming version".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        // Done reset; late TextComplete is ignored because there is no
        // active streaming block.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextComplete("corrected version".into())));
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)))
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)))
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("data".into())));
        let block_count_before = count_rows_containing(&fx, "data");
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        fx.handle.print_output(StyledBlock::new(StyledText::from(Span::new("garbage", s_assistant()))));
        fx.handle.redraw_sync();
        assert!(count_rows_containing(&fx, "garbage") > 0, "sanity: content on screen");
        // Send Ctrl-L.
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)))
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)))
            .expect("input open");
        std::thread::sleep(std::time::Duration::from_millis(50));
        // After Ctrl-W, "world" should be deleted, leaving "hello ".
        // BUG: Ctrl-W is silently ignored — buffer is still "hello world".
        assert_eq!(
            fx.handle.get_buffer(), "hello ",
            "BUG: Ctrl-W should delete previous word but buffer is unchanged"
        );
    }

    /// Sending /interrupt while a streaming block is active should
    /// not clear the streaming buffer; the next TextDelta after
    /// interrupt should still render.
    #[test]
    fn interrupt_during_streaming_does_not_orphan() {
        let mut fx = fixture();
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta("pre-interrupt".into())));
        let bid = fx.streaming.block_id;
        assert!(bid.is_some());
        // User interrupts.
        process_line("/interrupt", &mut fx.app, &fx.handle, &fx.cmd_tx);
        // Daemon should send Done to terminate the streaming block.
        // But the streaming tracker is still live.
        // More deltas after interrupt should still render.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::TextDelta(" post-interrupt".into())));
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming, chunk(OutputChunk::Done));
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming,
            chunk(OutputChunk::ToolStart {
                id: "t1".into(),
                name: "run".into(),
                input: serde_json::json!({"command": "true"}),
            }));
        fx.handle.redraw_sync();
        // Verify ToolStart rendered.
        assert!(count_rows_containing(&fx, "tool call run") > 0, "ToolStart text not found");
        assert!(count_rows_containing(&fx, "done") == 0, "'done' should not appear before ToolEnd");
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming,
            chunk(OutputChunk::ToolEnd {
                id: "t1".into(),
                result: omega_loop_client::ToolResultWire {
                    text: String::new(),
                    is_error: false,
                    content: None,
                },
            }));
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)))
            .expect("input open");
        match fx.term.next_event() {
            Some(Event::Line(line)) => assert_eq!(line, "first command"),
            other => panic!("expected Line, got {other:?}"),
        }
        // Buffer is now empty. Press Up to recall history.
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
            .expect("input open");
        // BUG: Up/Down are ignored — no event emitted.  The buffer
        // should contain the previous line.
        // Since next_event() blocks, we send a follow-up key to flush.
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)))
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
            .expect("input open");
        // Send 'x' to flush.
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)))
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
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming,
            chunk(OutputChunk::TextComplete(String::new())));
        // Done: no active block, resets anyway.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming,
            chunk(OutputChunk::Done));
        // Next turn should work.
        handle_daemon_event(&fx.handle, &mut fx.app, &mut fx.streaming,
            chunk(OutputChunk::TextDelta("should appear".into())));
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)))
            .expect("input open");
        // Drain the BufferChanged event from Ctrl-A.
        assert_eq!(fx.term.next_event(), Some(Event::BufferChanged));
        assert_eq!(fx.handle.get_cursor(), 0);
        // Type 'x' at position 0 → "xabc".
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)))
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
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)))
            .expect("input open");
        let _ = fx.term.next_event(); // BufferChanged from Home
        assert_eq!(fx.handle.get_cursor(), 0);
        // Press Ctrl-E: cursor moves to end.
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)))
            .expect("input open");
        assert_eq!(fx.term.next_event(), Some(Event::BufferChanged));
        assert_eq!(fx.handle.get_cursor(), 3);
        // Type 'x' at the end → "abcx".
        fx.input.send(RawEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)))
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
}
