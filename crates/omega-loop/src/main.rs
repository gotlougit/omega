//! # omega-loop — Core agent daemon
//!
//! Listens on a Unix socket, accepts connections from UI processes, and
//! manages agent sessions.  Each session runs the full agent loop (LLM +
//! tool orchestration) and farms out shell/filesystem operations to the
//! omega-sh daemon.
//!
//! ## Protocol (newline-delimited JSON)
//!
//! **Client → Server**
//! ```json
//! {"type":"run","session_id":"...","content":"...","config":{"stream":true,"think":false}}
//! {"type":"ask_response","session_id":"...","request_id":"...","answers":{"...":"..."}}
//! ```
//!
//! **Server → Client**
//! ```json
//! {"type":"Created","session_id":"...","session_name":"..."}
//! {"type":"Chunk","session_id":"...","chunk":<OutputChunk>}
//! ```

use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};
use tracing_subscriber::EnvFilter;

mod agent;
mod helpers;
mod runtime;
mod session;

use omega_core::core::{InputMessage, OutputChunk, SessionInfo};
use omega_llm::{AuthConfig, ContentBlock, LlmProvider, Message, MessageContent, OpenAIProvider};
use omega_tools::ToolRegistry;

use crate::agent::{AgentConfig, StandardAgent};
use crate::runtime::{AgentHandle, AgentRuntime};
use crate::session::{AgentSession, SessionStorage};
use omega_sh_client::OmegaClient;

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ClientRequest {
    #[serde(rename = "type")]
    msg_type: String,
    session_id: Option<String>,
    content: Option<String>,
    config: Option<SessionConfig>,
    request_id: Option<String>,
    answers: Option<HashMap<String, String>>,
    model: Option<String>,
    max_tokens: Option<u32>,
    /// Optional search filter for `list_sessions` (case-insensitive substring).
    query: Option<String>,
}

#[derive(Deserialize, Default)]
struct SessionConfig {
    #[serde(default)]
    think: bool,
    #[serde(default)]
    no_cache: bool,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type")]
enum ServerEvent {
    Created {
        session_id: String,
        session_name: String,
    },
    Chunk {
        session_id: String,
        chunk: OutputChunk,
    },
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    SessionResumed {
        session_id: String,
        session_name: String,
    },
    /// A single message from a resumed session's history, replayed so the
    /// client renders it exactly like a live turn.
    HistoryMessage {
        session_id: String,
        role: String,
        content: String,
    },
    ModelChanged {
        model: String,
    },
    SessionCompacted {
        session_id: String,
    },
    ModelList {
        models: Vec<String>,
    },
    SystemMsg {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// History replay + session listing helpers
// ---------------------------------------------------------------------------

/// Marker text the agent inserts into history when a turn is interrupted.
const INTERRUPT_MARKER: &str =
    "<vibe-working-agent-system>User interrupted this message</vibe-working-agent-system>";

/// A normalized, replayable message extracted from a session's history.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryReplay {
    role: String,
    content: String,
}

/// Extract the user-visible text of a message.
///
/// Block messages (tool results, thinking, images) contribute only their
/// `Text` blocks — everything else is skipped so the client never renders
/// raw tool plumbing as a chat turn.
fn message_text(msg: &Message) -> String {
    match &msg.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Normalize a session's persisted history into replayable turns.
///
/// Filters out the rows that would render as confusing or empty chat:
/// - empty user turns (e.g. tool-result messages, which are `user` role
///   but contain no visible text),
/// - assistant interrupt markers inserted when a user stopped a turn.
fn history_to_replay(messages: &[Message]) -> Vec<HistoryReplay> {
    let mut out = Vec::new();
    for msg in messages {
        if msg.role != "user" && msg.role != "assistant" {
            continue;
        }
        let content = message_text(msg);
        if content.trim().is_empty() {
            continue;
        }
        if content.trim() == INTERRUPT_MARKER {
            continue;
        }
        out.push(HistoryReplay {
            role: msg.role.clone(),
            content: content.trim().to_string(),
        });
    }
    out
}

/// Build a `SessionInfo` for a stored session, including a short preview of
/// its last meaningful message, plus the full replayed text used for search.
///
/// Returns `(info, search_blob)` where `search_blob` is every replayable
/// message joined together, so a query can match any point in the chat — not
/// just the last line.
fn build_session_info(
    storage: &crate::session::SessionStorage,
    session_id: &str,
    metadata: &crate::session::metadata::SessionMetadata,
) -> (SessionInfo, String) {
    let messages = storage.load_messages(session_id).unwrap_or_default();
    let replay = history_to_replay(&messages);
    let message_count = replay.len();
    let last_message = replay.last().map(|r| {
        if r.content.chars().count() > 80 {
            let truncated: String = r.content.chars().take(80).collect();
            format!("{truncated}…")
        } else {
            r.content.clone()
        }
    });
    let search_blob = replay
        .iter()
        .map(|r| r.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    (
        SessionInfo {
            session_id: session_id.to_string(),
            name: metadata.name.clone(),
            conversation_name: metadata.conversation_name.clone(),
            created_at: metadata.created_at.to_rfc3339(),
            updated_at: metadata.updated_at.to_rfc3339(),
            message_count,
            last_message,
        },
        search_blob,
    )
}

/// List top-level sessions with metadata, most-recently-updated first, with
/// an optional case-insensitive search filter.
fn list_sessions_filtered(
    storage: &crate::session::SessionStorage,
    query: Option<&str>,
) -> Vec<SessionInfo> {
    let mut sessions = match storage.list_sessions_with_metadata(true) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("list_sessions error: {e}");
            return Vec::new();
        }
    };

    // Nearest first — most recently updated at the top.
    sessions.sort_by_key(|(_, meta)| std::cmp::Reverse(meta.updated_at));
    let q = query.map(str::trim).filter(|q| !q.is_empty());
    let q = q.map(|q| q.to_lowercase());
    sessions
        .into_iter()
        .map(|(sid, meta)| build_session_info(storage, &sid, &meta))
        .filter(|(info, blob)| match &q {
            Some(q) => blob.to_lowercase().contains(q) || info.matches_query(q),
            None => true,
        })
        .map(|(info, _)| info)
        .collect()
}

// ---------------------------------------------------------------------------
// Shared infrastructure
// ---------------------------------------------------------------------------

fn create_llm_provider() -> Arc<dyn LlmProvider> {
    Arc::new(
        OpenAIProvider::with_auth_provider(|| async {
            let api_key = env::var("OPENAI_API_KEY")
                .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY environment variable not set"))?;
            let base_url = env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".to_string());
            Ok(AuthConfig::with_base_url(api_key, base_url))
        })
        .with_model(env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string()))
        .with_max_tokens(16384),
    )
}

fn create_tools() -> Result<Arc<ToolRegistry>> {
    let mut registry = ToolRegistry::new();
    let omega = OmegaClient::new()
        .with_session("omega-loop".to_string())
        .with_dir(
            env::current_dir()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_else(|_| "?".to_string()),
        );

    omega_sh_client::register_proxy_tools(&mut registry, omega);
    omega_tools::register_default_tools(&mut registry);

    Ok(Arc::new(registry))
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(
    stream: tokio::net::UnixStream,
    current_provider: Arc<std::sync::RwLock<Arc<dyn LlmProvider>>>,
    session_storage: Arc<crate::session::SessionStorage>,
    tools: Arc<ToolRegistry>,
    runtime: AgentRuntime,
) {
    let (reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(writer));

    // Broadcast channel for events that need to go out to the socket.
    // The writer task pumps this channel; session forwarders push into it.
    let (event_tx, _) = broadcast::channel(256);

    // Sessions created on THIS connection (session_id → handle).
    let sessions: Arc<Mutex<HashMap<String, AgentHandle>>> = Arc::new(Mutex::new(HashMap::new()));

    // --- writer task (sole writer to the socket) -------------------------
    let writer_handle = {
        let writer = writer.clone();
        let mut rx = event_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let json = match serde_json::to_string(&event) {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::warn!("serialize event: {e}");
                                continue;
                            }
                        };
                        let mut w = writer.lock().await;
                        if w.write_all(json.as_bytes()).await.is_err()
                            || w.write_all(b"\n").await.is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("event writer lagged by {n}");
                    }
                }
            }
        })
    };

    // --- reader task (reads JSON requests from the socket) ---------------
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: ClientRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("invalid request JSON: {e}");
                continue;
            }
        };

        // list_sessions doesn't need a session_id — handle it first.
        if req.msg_type == "list_sessions" {
            let list = list_sessions_filtered(&session_storage, req.query.as_deref());
            let _ = event_tx.send(ServerEvent::SessionList { sessions: list });
            continue;
        }

        // list_models doesn't need a session_id either — ask the current provider.
        if req.msg_type == "list_models" {
            let prov = {
                let lock = current_provider.read().unwrap();
                Arc::clone(&lock)
            };
            match prov.list_models().await {
                Ok(models) => {
                    let _ = event_tx.send(ServerEvent::ModelList { models });
                }
                Err(e) => {
                    tracing::warn!("list_models error: {e}");
                    let _ = event_tx.send(ServerEvent::ModelList { models: Vec::new() });
                }
            }
            continue;
        }

        let session_id = match req.session_id {
            Some(ref s) if !s.is_empty() => s.clone(),
            _ => {
                tracing::warn!("request missing session_id");
                continue;
            }
        };

        match req.msg_type.as_str() {
            "run" | "message" => {
                let mut sessions_lock = sessions.lock().await;
                let is_new = !sessions_lock.contains_key(&session_id);

                if is_new {
                    let config = req.config.unwrap_or_default();

                    // Apply model from request if provided (on session creation only)
                    if let Some(ref model) = req.model {
                        let max_tokens = req.max_tokens.unwrap_or(16384);
                        let prov = current_provider.read().unwrap().clone();
                        let new_provider = prov.create_variant(model, max_tokens);
                        *current_provider.write().unwrap() = new_provider;
                        tracing::info!(model = %model, "model set from run request");
                    }

                    // Always broadcast the current model so the client knows
                    // what the session is using.
                    {
                        let prov = current_provider.read().unwrap();
                        let _ = event_tx.send(ServerEvent::ModelChanged {
                            model: prov.model().to_string(),
                        });
                    }

                    // --- create session ----------------------------------
                    let storage = SessionStorage::with_dir("./sessions");
                    // Read system prompt from OMEGA_SYSTEM_PROMPT_PATH file, or empty
                    let system_prompt = env::var("OMEGA_SYSTEM_PROMPT_PATH")
                        .ok()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();

                    let agent_session = match AgentSession::new_with_storage(
                        &session_id,
                        "picrust",
                        "Picrust Agent",
                        "A coding agent",
                        &system_prompt,
                        storage,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(%session_id, "create session: {e}");
                            continue;
                        }
                    };

                    // --- build agent config -------------------------------
                    let mut agent_cfg = AgentConfig::new()
                        .with_tools(tools.clone())
                        .with_prompt_caching(!config.no_cache);

                    if config.think {
                        agent_cfg = agent_cfg.with_thinking(16000);
                    }

                    let agent =
                        StandardAgent::new(agent_cfg, current_provider.read().unwrap().clone());

                    // --- spawn agent task --------------------------------
                    let handle = runtime
                        .spawn(agent_session, |internals| agent.run(internals))
                        .await
                        .unwrap();

                    // --- forward output chunks to the event channel -------
                    let mut output_rx = handle.subscribe();
                    let ev_tx = event_tx.clone();
                    let sid = session_id.clone();
                    tokio::spawn(async move {
                        loop {
                            match output_rx.recv().await {
                                Ok(chunk) => {
                                    let event = ServerEvent::Chunk {
                                        session_id: sid.clone(),
                                        chunk,
                                    };
                                    if ev_tx.send(event).is_err() {
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Closed) => break,
                                Err(broadcast::error::RecvError::Lagged(n)) => {
                                    tracing::warn!(%sid, "output forwarder lagged by {n}");
                                }
                            }
                        }
                    });

                    // --- announce the new session -------------------------
                    let _ = event_tx.send(ServerEvent::Created {
                        session_id: session_id.clone(),
                        session_name: session_id.clone(),
                    });

                    tracing::info!(%session_id, "session created");

                    sessions_lock.insert(session_id.clone(), handle);
                }

                // --- forward user input to the agent ---------------------
                if let Some(content) = &req.content {
                    if !content.is_empty() {
                        if let Some(handle) = sessions_lock.get(&session_id) {
                            if let Err(e) = handle.send_input(content.clone()).await {
                                tracing::warn!(%session_id, "send input: {e}");
                            }
                        }
                    }
                }
            }

            "ask_response" => {
                let sessions_lock = sessions.lock().await;
                if let Some(handle) = sessions_lock.get(&session_id) {
                    if let (Some(request_id), Some(answers)) = (&req.request_id, &req.answers) {
                        let msg = InputMessage::UserQuestionResponse {
                            request_id: request_id.clone(),
                            answers: answers.clone(),
                        };
                        if let Err(e) = handle.send(msg).await {
                            tracing::warn!(%session_id, "send ask_response: {e}");
                        }
                    }
                }
            }

            "new_session" | "new" => {
                // Explicitly created on next run; just acknowledge.
                let _ = event_tx.send(ServerEvent::Created {
                    session_id: session_id.clone(),
                    session_name: session_id.clone(),
                });
            }

            "interrupt" => {
                let sessions_lock = sessions.lock().await;
                if let Some(handle) = sessions_lock.get(&session_id) {
                    let msg = InputMessage::Interrupt;
                    if let Err(e) = handle.send(msg).await {
                        tracing::warn!(%session_id, "send interrupt: {e}");
                    }
                }
            }

            "resume_session" => {
                if session_storage.session_exists(&session_id) {
                    let mut sessions_lock = sessions.lock().await;
                    if !sessions_lock.contains_key(&session_id) {
                        // Load existing session from disk
                        let agent_session = match crate::session::AgentSession::load_with_storage(
                            &session_id,
                            crate::session::SessionStorage::with_dir("./sessions"),
                        ) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(%session_id, "resume load session: {e}");
                                let _ = event_tx.send(ServerEvent::SystemMsg {
                                    message: format!("Cannot resume session: {e}"),
                                });
                                continue;
                            }
                        };

                        let prov = current_provider.read().unwrap().clone();
                        let agent_cfg = AgentConfig::new().with_tools(tools.clone());
                        let agent = StandardAgent::new(agent_cfg, prov);

                        let handle = runtime
                            .spawn(agent_session, |internals| agent.run(internals))
                            .await
                            .unwrap();

                        let mut output_rx = handle.subscribe();
                        let ev_tx = event_tx.clone();
                        let sid = session_id.clone();
                        tokio::spawn(async move {
                            loop {
                                match output_rx.recv().await {
                                    Ok(chunk) => {
                                        let event = ServerEvent::Chunk {
                                            session_id: sid.clone(),
                                            chunk,
                                        };
                                        if ev_tx.send(event).is_err() {
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                    Err(broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!(%sid, "output forwarder lagged by {n}");
                                    }
                                }
                            }
                        });

                        sessions_lock.insert(session_id.clone(), handle);
                    }

                    let _ = event_tx.send(ServerEvent::SessionResumed {
                        session_id: session_id.clone(),
                        session_name: session_id.clone(),
                    });

                    // Load message history and replay it to the client,
                    // rendered exactly like a live conversation (no empty
                    // turns, no tool plumbing).
                    if let Ok(messages) = session_storage.load_messages(&session_id) {
                        for replay in history_to_replay(&messages) {
                            let _ = event_tx.send(ServerEvent::HistoryMessage {
                                session_id: session_id.clone(),
                                role: replay.role,
                                content: replay.content,
                            });
                        }
                    }
                } else {
                    let _ = event_tx.send(ServerEvent::SystemMsg {
                        message: format!("Session '{session_id}' not found"),
                    });
                }
            }

            "set_model" => {
                if let Some(model) = &req.model {
                    let max_tokens = req.max_tokens.unwrap_or(16384);
                    let prov = current_provider.read().unwrap().clone();
                    let new_provider = prov.create_variant(model, max_tokens);
                    *current_provider.write().unwrap() = new_provider;
                    let _ = event_tx.send(ServerEvent::ModelChanged {
                        model: model.clone(),
                    });
                    tracing::info!(model = %model, "model changed");
                }
            }

            "compact" => {
                let mut sessions_lock = sessions.lock().await;
                if let Some(handle) = sessions_lock.get(&session_id) {
                    // Interrupt current processing
                    let _ = handle.interrupt().await;
                    // Remove the old handle (the agent task will terminate)
                    sessions_lock.remove(&session_id);
                }

                // Reload session from disk and re-spawn so the channel is fresh
                match crate::session::AgentSession::load_with_storage(
                    &session_id,
                    crate::session::SessionStorage::with_dir("./sessions"),
                ) {
                    Ok(agent_session) => {
                        let prov = current_provider.read().unwrap().clone();
                        let agent_cfg = AgentConfig::new().with_tools(tools.clone());
                        let agent = StandardAgent::new(agent_cfg, prov);

                        let handle = runtime
                            .spawn(agent_session, |internals| agent.run(internals))
                            .await
                            .unwrap();

                        // Forward output chunks to the event channel
                        let mut output_rx = handle.subscribe();
                        let ev_tx = event_tx.clone();
                        let sid = session_id.clone();
                        tokio::spawn(async move {
                            loop {
                                match output_rx.recv().await {
                                    Ok(chunk) => {
                                        let event = ServerEvent::Chunk {
                                            session_id: sid.clone(),
                                            chunk,
                                        };
                                        if ev_tx.send(event).is_err() {
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                    Err(broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!(%sid, "output forwarder lagged by {n}");
                                    }
                                }
                            }
                        });

                        sessions_lock.insert(session_id.clone(), handle);

                        let _ = event_tx.send(ServerEvent::SessionCompacted {
                            session_id: session_id.clone(),
                        });
                        let _ = event_tx.send(ServerEvent::SystemMsg {
                            message: format!("Session '{}' compacted and re-created", session_id),
                        });
                        tracing::info!(%session_id, "session compacted and re-created");
                    }
                    Err(e) => {
                        tracing::error!(%session_id, "compact reload session: {e}");
                        let _ = event_tx.send(ServerEvent::SystemMsg {
                            message: format!("Cannot compact session: {e}"),
                        });
                    }
                }
            }

            other => {
                tracing::warn!("unknown request type: {other}");
            }
        }
    }

    // --- connection closed — shut down all sessions -----------------------
    {
        let mut sessions_lock = sessions.lock().await;
        for (sid, handle) in sessions_lock.drain() {
            let _ = handle.shutdown().await;
            tracing::info!(session_id = %sid, "shut down on disconnect");
        }
    }
    drop(event_tx); // signals the writer task to stop
    let _ = writer_handle.await; // wait for writer to finish
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let llm = create_llm_provider();
    let tools = create_tools()?;
    let runtime = AgentRuntime::new();

    // Track the currently-active LLM provider so we can change models at runtime.
    let current_provider: Arc<std::sync::RwLock<Arc<dyn LlmProvider>>> =
        Arc::new(std::sync::RwLock::new(llm));

    // Global shared SessionStorage for listing/resuming sessions.
    let session_storage = Arc::new(crate::session::SessionStorage::with_dir("./sessions"));

    let socket_path =
        env::var("OMEGA_LOOP_SOCKET_PATH").unwrap_or_else(|_| "/tmp/omega-loop.sock".to_string());

    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Cannot bind to {socket_path}"))?;

    let cwd = env::current_dir()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|_| "?".to_string());
    {
        let prov = current_provider.read().unwrap();
        tracing::info!(socket = %socket_path, cwd = %cwd, model = %prov.model(), "omega-loop started");
        eprintln!("omega-loop ({}) listening on {socket_path}", prov.model());
    }

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tracing::debug!(peer = ?addr, "accepted connection");
                let current_provider = current_provider.clone();
                let session_storage = session_storage.clone();
                let tools = tools.clone();
                let runtime = runtime.clone();
                tokio::spawn(handle_connection(
                    stream,
                    current_provider,
                    session_storage,
                    tools,
                    runtime,
                ));
            }
            Err(e) => {
                tracing::error!("accept error: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omega_llm::{ContentBlock, Message};
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // history_to_replay — resume replay normalization
    // -----------------------------------------------------------------------

    #[test]
    fn replay_keeps_simple_user_and_assistant_turns() {
        let messages = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("how are you?"),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 3);
        assert_eq!(replay[0].role, "user");
        assert_eq!(replay[0].content, "hello");
        assert_eq!(replay[1].role, "assistant");
        assert_eq!(replay[1].content, "hi there");
    }

    /// Tool results are stored as `user` messages with non-text blocks.
    /// They must NOT be replayed as empty user turns (the reported bug).
    #[test]
    fn replay_skips_empty_user_turns_from_tool_results() {
        let messages = vec![
            Message::user("actual prompt"),
            Message::assistant_with_blocks(vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                signature: None,
            }]),
            // The user turn that carries the tool result — role "user",
            // but only ToolResult blocks (no visible text).
            Message::user_with_blocks(vec![ContentBlock::tool_result("t1", "file1\nfile2", false)]),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 1, "only the real user prompt should survive");
        assert_eq!(replay[0].content, "actual prompt");
    }

    #[test]
    fn replay_skips_empty_text_messages() {
        let messages = vec![
            Message::user(""),
            Message::assistant("   "),
            Message::user("real content"),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].content, "real content");
    }

    /// Interrupt markers inserted by the agent are noise for a resumed chat.
    #[test]
    fn replay_skips_interrupt_marker() {
        let messages = vec![
            Message::user("do the thing"),
            Message::assistant(INTERRUPT_MARKER),
            Message::user("no wait, do the other thing"),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 2);
        assert!(
            replay.iter().all(|r| r.content != INTERRUPT_MARKER),
            "interrupt marker must not be replayed"
        );
    }

    /// Assistant replies with mixed blocks (thinking + text + tool use)
    /// contribute only their text.
    #[test]
    fn replay_assistant_blocks_only_use_text() {
        let messages = vec![Message::assistant_with_blocks(vec![
            ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: "sig".into(),
            },
            ContentBlock::Text {
                text: "final answer".into(),
                cache_control: None,
            },
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                signature: None,
            },
        ])];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].content, "final answer");
    }

    #[test]
    fn replay_ignores_non_user_assistant_roles() {
        let messages = vec![
            Message::user("hello"),
            Message {
                role: "system".into(),
                content: omega_llm::MessageContent::Text("sys".into()),
            },
            Message::assistant("world"),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 2);
    }

    #[test]
    fn replay_trims_whitespace() {
        let messages = vec![Message::user("  padded  ")];
        let replay = history_to_replay(&messages);
        assert_eq!(replay[0].content, "padded");
    }

    // -----------------------------------------------------------------------
    // list_sessions_filtered — recency sort + search
    // -----------------------------------------------------------------------

    fn storage_with_sessions(names: &[(&str, &str)]) -> (SessionStorage, TempDir) {
        let temp = TempDir::new().unwrap();
        let storage = SessionStorage::with_dir(temp.path());
        for (id, conv) in names {
            let mut meta = crate::session::metadata::SessionMetadata::new(*id, "picrust", "Picrust Agent", "d");
            if !conv.is_empty() {
                meta.set_conversation_name(*conv);
            }
            storage.save_metadata(&meta).unwrap();
        }
        (storage, temp)
    }

    /// Sessions must come back most-recently-updated first so the TUI can
    /// present a "nearest first" resume list.
    #[test]
    fn list_sessions_sorted_by_recency() {
        let (storage, _t) = storage_with_sessions(&[("old", "Old chat"), ("new", "New chat")]);
        // Force a distinct updated_at for "new" (touching bumps the timestamp).
        {
            let mut meta = storage.load_metadata("new").unwrap();
            meta.touch();
            storage.save_metadata(&meta).unwrap();
        }
        let list = list_sessions_filtered(&storage, None);
        assert_eq!(list.len(), 2);
        // "new" was touched after "old" was created → it must be first.
        assert_eq!(list[0].session_id, "new");
        assert_eq!(list[1].session_id, "old");
    }

    #[test]
    fn list_sessions_query_filters() {
        let (storage, _t) = storage_with_sessions(&[
            ("sess-fix", "Fix the build"),
            ("sess-tui", "TUI tests"),
        ]);
        let list = list_sessions_filtered(&storage, Some("build"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "sess-fix");
    }

    #[test]
    fn list_sessions_query_matches_last_message() {
        let (storage, _t) = storage_with_sessions(&[("sess-a", ""), ("sess-b", "")]);
        // Add a message to sess-a so its preview contains the searchable text.
        storage
            .append_message("sess-a", &Message::user("refactor the parser"))
            .unwrap();
        let list = list_sessions_filtered(&storage, Some("parser"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "sess-a");
        assert!(list[0].last_message.as_deref().unwrap().contains("parser"));
    }

    #[test]
    fn list_sessions_query_blank_returns_all() {
        let (storage, _t) = storage_with_sessions(&[("s1", "A"), ("s2", "B")]);
        let list = list_sessions_filtered(&storage, Some("   "));
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_sessions_excludes_subagents() {
        let (storage, _t) = storage_with_sessions(&[("parent", "Parent")]);
        let sub = crate::session::metadata::SessionMetadata::new_subagent(
            "child",
            "helper",
            "Child",
            "d",
            "parent",
            "tool_1",
        );
        storage.save_metadata(&sub).unwrap();
        let list = list_sessions_filtered(&storage, None);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "parent");
    }

    #[test]
    fn session_info_last_message_truncated() {
        let (storage, _t) = storage_with_sessions(&[("sess", "")]);
        let long = "x".repeat(200);
        storage.append_message("sess", &Message::user(&long)).unwrap();
        let meta = storage.load_metadata("sess").unwrap();
        let (info, blob) = build_session_info(&storage, "sess", &meta);
        let preview = info.last_message.unwrap();
        assert!(preview.chars().count() <= 81, "preview must be truncated");
        assert!(preview.ends_with('…'));
        assert!(!blob.is_empty(), "search blob must contain the message");
    }

    /// A query matching an EARLIER message (not the last one) still finds the
    /// session — search covers the whole conversation.
    #[test]
    fn list_sessions_query_matches_earlier_message() {
        let (storage, _t) = storage_with_sessions(&[("sess-a", ""), ("sess-b", "")]);
        storage.append_message("sess-a", &Message::user("lets discuss interrupt steering")).unwrap();
        storage.append_message("sess-a", &Message::assistant("sure")).unwrap();
        storage.append_message("sess-b", &Message::user("unrelated")).unwrap();
        storage.append_message("sess-b", &Message::assistant("ok")).unwrap();

        let list = list_sessions_filtered(&storage, Some("steering"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "sess-a");
        // The last message of sess-a is "sure", which doesn't match, so this
        // proves the earlier user message was searched.
        assert_eq!(list[0].last_message.as_deref(), Some("sure"));
    }
}
