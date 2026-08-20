//! Minimal omega-loop daemon client for the web control panel.
//!
//! The web server talks to the agent daemon over its Unix socket to start
//! sessions (activating a project worktree), send chat messages, interrupt
//! running turns, and stream live output to the browser. Every operation
//! opens a short-lived connection (the daemon keeps live sessions in a
//! daemon-global registry, so closing a connection never kills a session —
//! that's what lets a web page come and go freely).

use omega_loop_client::{connect_to, DaemonWriter, ServerEvent};
use omega_projects::ActiveProject;
use std::future::Future;
use std::time::Duration;

const DEFAULT_SOCKET: &str = "/tmp/omega-loop.sock";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const COMPACT_TIMEOUT: Duration = Duration::from_secs(125);

fn socket_path() -> String {
    std::env::var("OMEGA_LOOP_SOCKET_PATH").unwrap_or_else(|_| DEFAULT_SOCKET.to_string())
}

/// Open a short-lived connection to the daemon.
async fn open_conn() -> Result<(omega_loop_client::DaemonReader, DaemonWriter), String> {
    with_timeout(
        CONNECT_TIMEOUT,
        "connecting to omega-loop",
        connect_to(&socket_path()),
    )
    .await?
    .map_err(|e| format!("cannot reach omega-loop at {}: {e:#}", socket_path()))
}

async fn with_timeout<T, E>(
    duration: Duration,
    operation: &str,
    future: impl Future<Output = Result<T, E>>,
) -> Result<Result<T, E>, String> {
    tokio::time::timeout(duration, future)
        .await
        .map_err(|_| format!("timed out {operation}"))
}

/// Mark a brand-new session as being bound to a project's worktree: the
/// daemon creates (or reuses) the worktree, spawns the session, and, once
/// we see the `ProjectActive` confirmation, the session is live. Returns
/// `Ok(Ok(()))` on success or `Ok(Err(msg))` if the daemon refused.
pub async fn create_session_on_project(
    project: &str,
    session_id: &str,
) -> Result<Result<(), String>, String> {
    let (mut reader, mut writer) = open_conn().await?;
    writer
        .send_activate_project(session_id, project)
        .await
        .map_err(|e| format!("failed to request session start: {e:#}"))?;
    with_timeout(CONTROL_TIMEOUT, "starting the session", async {
        while let Some(event) = reader.recv_event().await.map_err(|e| e.to_string())? {
            match event {
                ServerEvent::ProjectActive { .. } => return Ok(Ok(())),
                ServerEvent::SystemMsg { message } if message.contains("Cannot") => {
                    return Ok(Err(message))
                }
                _ => {}
            }
        }
        Err("omega-loop closed the connection before confirming the session".to_string())
    })
    .await?
}

/// Discover the models offered by omega-loop's configured provider.
pub async fn list_models() -> Result<Vec<String>, String> {
    let (mut reader, mut writer) = open_conn().await?;
    writer
        .send_list_models()
        .await
        .map_err(|e| format!("failed to request models: {e:#}"))?;
    with_timeout(CONTROL_TIMEOUT, "listing models", async {
        while let Some(event) = reader.recv_event().await.map_err(|e| e.to_string())? {
            match event {
                ServerEvent::ModelList { models } => return Ok(models),
                ServerEvent::SystemMsg { message } if message.contains("Cannot") => {
                    return Err(message)
                }
                _ => {}
            }
        }
        Err("omega-loop closed the connection before listing models".to_string())
    })
    .await?
}

/// Change exactly one session and wait for the daemon's canonical event.
pub async fn set_model(session_id: &str, model: &str) -> Result<String, String> {
    let (mut reader, mut writer) = open_conn().await?;
    writer
        .send_set_model(session_id, model, None)
        .await
        .map_err(|e| format!("failed to request model change: {e:#}"))?;
    with_timeout(CONTROL_TIMEOUT, "changing the session model", async {
        while let Some(event) = reader.recv_event().await.map_err(|e| e.to_string())? {
            match event {
                ServerEvent::ModelChanged {
                    session_id: confirmed,
                    model,
                } if confirmed.is_empty() || confirmed == session_id => return Ok(model),
                ServerEvent::SystemMsg { message } if message.contains("Cannot") => {
                    return Err(message)
                }
                _ => {}
            }
        }
        Err("omega-loop closed the connection before confirming the model".to_string())
    })
    .await?
}

/// Compact exactly one session and wait for the canonical completion event.
pub async fn compact(session_id: &str) -> Result<(), String> {
    let (mut reader, mut writer) = open_conn().await?;
    writer
        .send_compact(session_id)
        .await
        .map_err(|e| format!("failed to request compaction: {e:#}"))?;
    with_timeout(COMPACT_TIMEOUT, "compacting the session", async {
        while let Some(event) = reader.recv_event().await.map_err(|e| e.to_string())? {
            match event {
                ServerEvent::SessionCompacted {
                    session_id: confirmed,
                } if confirmed == session_id => return Ok(()),
                ServerEvent::SystemMsg { message }
                    if message.contains("Cannot compact")
                        || message.contains("history was compacted") =>
                {
                    return Err(message)
                }
                _ => {}
            }
        }
        Err("omega-loop closed the connection before confirming compaction".to_string())
    })
    .await?
}

/// Send a chat message for `session_id`. The daemon delivers it to the live
/// (daemon-global) session; the browser's SSE stream carries the reply. When
/// `project` is given, the session is bound to that project's worktree first
/// (used for a brand-new session). Closing this connection does not stop the
/// session — it keeps running detached.
pub async fn send_message(
    session_id: &str,
    content: &str,
    project: Option<ActiveProject>,
) -> Result<(), String> {
    let (_reader, mut writer) = open_conn().await?;
    writer
        .send_run(
            session_id,
            content,
            &Default::default(),
            None,
            project.as_ref(),
            None,
        )
        .await
        .map_err(|e| format!("failed to send message: {e:#}"))?;
    Ok(())
}

/// Interrupt the running turn of `session_id`.
pub async fn interrupt(session_id: &str) -> Result<(), String> {
    let (mut _reader, mut writer) = open_conn().await?;
    writer
        .send_interrupt(session_id)
        .await
        .map_err(|e| format!("failed to interrupt: {e:#}"))?;
    Ok(())
}

/// Connect for streaming: sends `resume_session` so the daemon replays
/// history and forwards any subsequent live output. Returns the reader to
/// pump, plus the writer (dropping both closes the SSE stream).
pub async fn open_stream(
    session_id: &str,
) -> Result<(omega_loop_client::DaemonReader, DaemonWriter), String> {
    let (reader, mut writer) = open_conn().await?;
    writer
        .send_resume_session(session_id)
        .await
        .map_err(|e| format!("failed to resume session: {e:#}"))?;
    Ok((reader, writer))
}
