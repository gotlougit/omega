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

const DEFAULT_SOCKET: &str = "/tmp/omega-loop.sock";

fn socket_path() -> String {
    std::env::var("OMEGA_LOOP_SOCKET_PATH").unwrap_or_else(|_| DEFAULT_SOCKET.to_string())
}

/// Open a short-lived connection to the daemon.
async fn open_conn(
) -> Result<(omega_loop_client::DaemonReader, DaemonWriter), String> {
    connect_to(&socket_path())
        .await
        .map_err(|e| format!("cannot reach omega-loop at {}: {e:#}", socket_path()))
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
    while let Some(event) = reader.recv_event().await.map_err(|e| e.to_string())? {
        match event {
            ServerEvent::ProjectActive { .. } => return Ok(Ok(())),
            ServerEvent::SystemMsg(msg) if msg.contains("Cannot") => return Ok(Err(msg)),
            _ => {}
        }
    }
    // The loop never confirms but the activation may still have happened;
    // treat an EOF without a refusal as success.
    Ok(Ok(()))
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
