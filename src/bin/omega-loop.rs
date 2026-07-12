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

use picrust::{
    agent::{AgentConfig, StandardAgent},
    llm::{AuthConfig, LlmProvider, OpenAIProvider},
    omega_client::proxy::{BashProxy, EditProxy, GlobProxy, GrepProxy, ReadProxy, WriteProxy},
    omega_client::OmegaClient,
    runtime::{AgentHandle, AgentRuntime},
    session::{AgentSession, SessionStorage},
    tools::{AskUserQuestionTool, ToolRegistry},
};

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
}

#[derive(Deserialize, Default)]
struct SessionConfig {
    #[serde(default)]
    stream: bool,
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
        chunk: picrust::core::OutputChunk,
    },
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

    registry.register(ReadProxy::new(omega.clone()));
    registry.register(WriteProxy::new(omega.clone()));
    registry.register(EditProxy::new(omega.clone()));
    registry.register(BashProxy::new(omega.clone()));
    registry.register(GlobProxy::new(omega.clone()));
    registry.register(GrepProxy::new(omega.clone()));
    registry.register(AskUserQuestionTool::new());

    Ok(Arc::new(registry))
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(
    stream: tokio::net::UnixStream,
    llm: Arc<dyn LlmProvider>,
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

                    // --- create session ----------------------------------
                    let storage = SessionStorage::with_dir("./sessions");
                    let agent_session = match AgentSession::new_with_storage(
                        &session_id,
                        "picrust",
                        "Picrust Agent",
                        "A coding agent",
                        "", // system prompt – baked into the session file
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
                        .with_streaming(config.stream)
                        .with_prompt_caching(!config.no_cache)
                        .with_auto_name(true)
                        .with_dangerous_skip_permissions(true);

                    if config.think {
                        agent_cfg = agent_cfg.with_thinking(16000);
                    }

                    let agent = StandardAgent::new(agent_cfg, llm.clone());

                    // --- spawn agent task --------------------------------
                    let handle = runtime
                        .spawn(agent_session, |internals| agent.run(internals))
                        .await;

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
                        let msg = picrust::core::InputMessage::UserQuestionResponse {
                            request_id: request_id.clone(),
                            answers: answers.clone(),
                        };
                        if let Err(e) = handle.send(msg).await {
                            tracing::warn!(%session_id, "send ask_response: {e}");
                        }
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

    let socket_path =
        env::var("OMEGA_LOOP_SOCKET_PATH").unwrap_or_else(|_| "/tmp/omega-loop.sock".to_string());

    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Cannot bind to {socket_path}"))?;

    let cwd = env::current_dir()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|_| "?".to_string());
    tracing::info!(socket = %socket_path, cwd = %cwd, model = %llm.model(), "omega-loop started");
    eprintln!("omega-loop ({}) listening on {socket_path}", llm.model());

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tracing::debug!(peer = ?addr, "accepted connection");
                let llm = llm.clone();
                let tools = tools.clone();
                let runtime = runtime.clone();
                tokio::spawn(handle_connection(stream, llm, tools, runtime));
            }
            Err(e) => {
                tracing::error!("accept error: {e}");
            }
        }
    }
}
