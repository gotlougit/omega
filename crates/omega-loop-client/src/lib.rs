//! # omega-loop client — connect to the agent daemon from a UI process
//!
//! The main entry points are [`connect`] and [`connect_to`], which return a
//! `(DaemonReader, DaemonWriter)` pair.  Use the writer to send commands and
//! the reader to receive events.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Re-export core so UI crates can construct canonical chunk fixtures without
/// adding a second direct protocol dependency.
pub use omega_core;

/// Re-exported project types so clients only need to depend on this crate.
pub use omega_projects::{ActiveProject, ProjectInfo};

// Canonical wire types are shared with the daemon.  Re-export them so UI
// crates retain the omega-loop-client-only dependency surface.
pub use omega_core::core::OutputChunk;
pub use omega_protocol::daemon::{
    tool_result_text, ClientRequest, ServerEvent, SessionConfig, WireChunk,
};

/// Read half of a daemon connection — reads events from the socket.
pub struct DaemonReader {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
}

/// Write half of a daemon connection — sends commands to the socket.
pub struct DaemonWriter {
    writer: tokio::io::WriteHalf<UnixStream>,
}

// ---------------------------------------------------------------------------
// Free functions — connect to the daemon
// ---------------------------------------------------------------------------

/// Connect to the omega-loop daemon using `OMEGA_LOOP_SOCKET_PATH` env or default.
pub async fn connect() -> Result<(DaemonReader, DaemonWriter)> {
    let path = std::env::var("OMEGA_LOOP_SOCKET_PATH")
        .unwrap_or_else(|_| "/tmp/omega-loop.sock".to_string());
    connect_to(&path).await
}

/// Connect to the omega-loop daemon at a specific socket path.
pub async fn connect_to(path: &str) -> Result<(DaemonReader, DaemonWriter)> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("Cannot connect to omega-loop at {path}"))?;
    let (reader, writer) = tokio::io::split(stream);
    Ok((
        DaemonReader {
            reader: BufReader::new(reader),
        },
        DaemonWriter { writer },
    ))
}

// ---------------------------------------------------------------------------
// DaemonReader — read events from the daemon
// ---------------------------------------------------------------------------

impl DaemonReader {
    /// Read the next event from the daemon. Returns `None` on EOF.
    pub async fn recv_event(&mut self) -> Result<Option<ServerEvent>> {
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(None);
            }
            let line = line.trim();
            if !line.is_empty() {
                let event = ServerEvent::from_json_line(line)?;
                return Ok(Some(event));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DaemonWriter — send commands to the daemon
// ---------------------------------------------------------------------------

impl DaemonWriter {
    /// Send a `run` request (creates session if new, then sends the message).
    /// If `model` is `Some`, it will be applied on session creation.
    /// If `project` is `Some`, the session is bound to that project's
    /// worktree (all tool calls run inside it).
    /// If `role` is `Some`, a *new* session is created using that role's
    /// system prompt (the named alternative prompt configured in NixOS)
    /// instead of the default.
    pub async fn send_run(
        &mut self,
        session_id: &str,
        content: &str,
        config: &SessionConfig,
        model: Option<&str>,
        project: Option<&ActiveProject>,
        role: Option<&str>,
    ) -> Result<()> {
        let mut request = ClientRequest::new("run");
        request.session_id = Some(session_id.to_string());
        request.content = Some(content.to_string());
        request.config = Some(config.clone());
        request.model = model.map(str::to_string);
        request.project = project.cloned();
        request.role = role.map(str::to_string);
        self.write_request(&request).await
    }

    /// Request the list of available sessions from the daemon.
    ///
    /// `query` is an optional case-insensitive substring filter applied by
    /// the daemon against session id / name / conversation name / last
    /// message. The result is sorted most-recently-updated first.
    pub async fn send_list_sessions(&mut self, query: Option<&str>) -> Result<()> {
        let mut request = ClientRequest::new("list_sessions");
        request.query = query
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_string);
        self.write_request(&request).await
    }

    /// Request the list of available models from the daemon.
    pub async fn send_list_models(&mut self) -> Result<()> {
        self.write_request(&ClientRequest::new("list_models")).await
    }

    /// Request the list of registered projects from the daemon.
    pub async fn send_list_projects(&mut self) -> Result<()> {
        self.write_request(&ClientRequest::new("list_projects"))
            .await
    }

    /// Request the list of available roles (named alternative system
    /// prompts) from the daemon.
    pub async fn send_list_roles(&mut self) -> Result<()> {
        self.write_request(&ClientRequest::new("list_roles")).await
    }

    /// Activate a project for a session: `spec` is a registered project
    /// name or a git URL. The daemon clones the repo if needed and creates
    /// a dedicated git worktree for the session.
    pub async fn send_activate_project(&mut self, session_id: &str, spec: &str) -> Result<()> {
        let mut request = ClientRequest::new("activate_project");
        request.session_id = Some(session_id.to_string());
        request.spec = Some(spec.to_string());
        self.write_request(&request).await
    }

    /// Resume an existing session by ID.
    pub async fn send_resume_session(&mut self, session_id: &str) -> Result<()> {
        let mut request = ClientRequest::new("resume_session");
        request.session_id = Some(session_id.to_string());
        self.write_request(&request).await
    }

    /// Change the model for a session. A `max_tokens` of `Some(n)` overrides
    /// the configured output cap; `None` inherits the daemon's current value
    /// (from `OPENAI_MAX_TOKENS`, if set).
    pub async fn send_set_model(
        &mut self,
        session_id: &str,
        model: &str,
        max_tokens: Option<u32>,
    ) -> Result<()> {
        let mut request = ClientRequest::new("set_model");
        request.session_id = Some(session_id.to_string());
        request.model = Some(model.to_string());
        request.max_tokens = max_tokens;
        self.write_request(&request).await
    }

    /// Compact the current session (summarize/trim history).
    pub async fn send_compact(&mut self, session_id: &str) -> Result<()> {
        let mut request = ClientRequest::new("compact");
        request.session_id = Some(session_id.to_string());
        self.write_request(&request).await
    }

    /// Interrupt the current LLM response.
    pub async fn send_interrupt(&mut self, session_id: &str) -> Result<()> {
        let mut request = ClientRequest::new("interrupt");
        request.session_id = Some(session_id.to_string());
        self.write_request(&request).await
    }

    async fn write_request(&mut self, request: &ClientRequest) -> Result<()> {
        let json = serde_json::to_string(request)?;
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_created_event() {
        let json = r#"{"type":"Created","session_id":"test-123","session_name":"Test Session"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::Created {
                session_id,
                session_name,
            } => {
                assert_eq!(session_id, "test-123");
                assert_eq!(session_name, "Test Session");
            }
            _ => panic!("Expected Created event"),
        }
    }

    #[test]
    fn test_parse_session_list_event() {
        let json = r#"{"type":"SessionList","sessions":[{"session_id":"sess1","name":"A","created_at":"2025-01-01T00:00:00Z","updated_at":"2025-01-02T00:00:00Z","message_count":2}]}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::SessionList { sessions } => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].session_id, "sess1");
                assert_eq!(sessions[0].message_count, 2);
            }
            _ => panic!("Expected SessionList event"),
        }
    }

    #[test]
    fn test_parse_session_list_with_full_info() {
        let json = r#"{"type":"SessionList","sessions":[{"session_id":"sess1","name":"omega-tui","conversation_name":"Fix build","created_at":"2025-01-01T00:00:00Z","updated_at":"2025-01-02T00:00:00Z","message_count":5,"last_message":"hello"}]}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::SessionList { sessions } => {
                assert_eq!(sessions[0].conversation_name.as_deref(), Some("Fix build"));
                assert_eq!(sessions[0].last_message.as_deref(), Some("hello"));
            }
            _ => panic!("Expected SessionList event"),
        }
    }

    #[test]
    fn test_parse_history_message_event() {
        let json = r#"{"type":"HistoryMessage","session_id":"sess1","role":"user","content":"hello there"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::HistoryMessage {
                session_id,
                role,
                content,
            } => {
                assert_eq!(session_id, "sess1");
                assert_eq!(role, "user");
                assert_eq!(content, "hello there");
            }
            _ => panic!("Expected HistoryMessage event"),
        }
    }

    #[test]
    fn test_parse_model_changed_event() {
        let json = r#"{"type":"ModelChanged","model":"gpt-4o"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::ModelChanged { session_id, model } => {
                assert!(session_id.is_empty());
                assert_eq!(model, "gpt-4o");
            }
            _ => panic!("Expected ModelChanged event"),
        }
    }

    #[test]
    fn test_parse_model_changed_event_with_session_identity() {
        let json = r#"{"type":"ModelChanged","session_id":"sess1","model":"gpt-4o"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::ModelChanged { session_id, model } => {
                assert_eq!(session_id, "sess1");
                assert_eq!(model, "gpt-4o");
            }
            _ => panic!("Expected ModelChanged event"),
        }
    }

    #[test]
    fn test_parse_system_msg_event() {
        let json = r#"{"type":"SystemMsg","message":"Hello world"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::SystemMsg { message } => {
                assert_eq!(message, "Hello world");
            }
            _ => panic!("Expected SystemMsg event"),
        }
    }

    #[test]
    fn test_parse_role_list_event() {
        let json = r#"{"type":"RoleList","roles":[{"name":"reverseengineer"}]}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::RoleList { roles } => {
                assert_eq!(roles.len(), 1);
                assert_eq!(roles[0].name, "reverseengineer");
            }
            _ => panic!("Expected RoleList event"),
        }
    }

    #[test]
    fn test_send_list_roles_json_shape() {
        let json = serde_json::to_value(ClientRequest::new("list_roles")).unwrap();
        assert_eq!(json["type"], "list_roles");
    }

    #[test]
    fn test_parse_unknown_event() {
        let json = r#"{"type":"UnknownType","foo":"bar"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::Unknown => {}
            _ => panic!("Expected Unknown event"),
        }
    }

    #[test]
    fn test_parse_project_list_event() {
        let json = r#"{"type":"ProjectList","projects":[{"name":"omega","url":"https://example.com/omega.git","default_branch":"main","created_at":"2025-01-01T00:00:00Z"}]}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::ProjectList { projects } => {
                assert_eq!(projects.len(), 1);
                assert_eq!(projects[0].name, "omega");
                assert_eq!(projects[0].url, "https://example.com/omega.git");
                assert_eq!(projects[0].default_branch.as_deref(), Some("main"));
            }
            _ => panic!("Expected ProjectList event"),
        }
    }

    #[test]
    fn test_parse_project_active_event() {
        let json = r#"{"type":"ProjectActive","project":{"name":"omega","url":"https://example.com/omega.git","created_at":"2025-01-01T00:00:00Z"},"worktree_path":"/tmp/projects/worktrees/omega/sess-1-ab12cd","branch":"omega/sess-1-ab12cd"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::ProjectActive {
                project,
                worktree_path,
                branch,
            } => {
                assert_eq!(project.name, "omega");
                assert_eq!(worktree_path, "/tmp/projects/worktrees/omega/sess-1-ab12cd");
                assert_eq!(branch, "omega/sess-1-ab12cd");
            }
            _ => panic!("Expected ProjectActive event"),
        }
    }

    #[test]
    fn test_output_chunk_parse_text_delta() {
        let json = r#"{"TextDelta":"Hello"}"#;
        let chunk: WireChunk = serde_json::from_str(json).unwrap();
        match chunk {
            WireChunk::Known(OutputChunk::TextDelta(s)) => assert_eq!(s, "Hello"),
            _ => panic!("Expected TextDelta"),
        }
    }

    #[test]
    fn test_output_chunk_parse_done() {
        let chunk: WireChunk = serde_json::from_str(r#""Done""#).unwrap();
        match chunk {
            WireChunk::Known(OutputChunk::Done) => {}
            _ => panic!("Expected Done"),
        }
    }

    #[test]
    fn test_session_config_default() {
        let config = SessionConfig::default();
        assert!(config.stream);
        assert!(!config.think);
        assert!(!config.no_cache);
    }

    #[test]
    fn test_server_event_from_invalid_json() {
        let result = ServerEvent::from_json_line("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_send_run_json_shape() {
        let mut request = ClientRequest::new("run");
        request.session_id = Some("sess-1".into());
        request.content = Some("Hello".into());
        request.config = Some(SessionConfig::default());
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["type"], "run");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["content"], "Hello");
        assert_eq!(json["config"]["stream"], true);
        assert_eq!(json["config"]["think"], false);
    }

    #[test]
    fn test_send_run_with_project_json_shape() {
        let active = ActiveProject {
            project: ProjectInfo {
                name: "omega".into(),
                url: "https://example.com/omega.git".into(),
                default_branch: Some("main".into()),
                created_at: Default::default(),
            },
            worktree_path: "/tmp/wt/omega/sess-1".into(),
            branch: "omega/sess-1-abc123".into(),
        };
        let mut request = ClientRequest::new("run");
        request.project = Some(active);
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["project"]["project"]["name"], "omega");
        assert_eq!(json["project"]["worktree_path"], "/tmp/wt/omega/sess-1");
        assert_eq!(json["project"]["branch"], "omega/sess-1-abc123");
    }

    #[test]
    fn test_send_activate_project_json_shape() {
        let mut request = ClientRequest::new("activate_project");
        request.session_id = Some("sess-1".into());
        request.spec = Some("https://example.com/omega.git".into());
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["type"], "activate_project");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["spec"], "https://example.com/omega.git");
    }

    #[test]
    fn test_send_list_projects_json_shape() {
        let json = serde_json::to_value(ClientRequest::new("list_projects")).unwrap();
        assert_eq!(json["type"], "list_projects");
    }

    #[test]
    fn test_send_list_sessions_json_shape() {
        let json = serde_json::to_value(ClientRequest::new("list_sessions")).unwrap();
        assert_eq!(json["type"], "list_sessions");
    }

    #[test]
    fn test_send_list_sessions_with_query() {
        let mut request = ClientRequest::new("list_sessions");
        request.query = Some("build".into());
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["type"], "list_sessions");
        assert_eq!(json["query"], "build");
    }

    #[test]
    fn test_send_resume_session_json_shape() {
        let mut request = ClientRequest::new("resume_session");
        request.session_id = Some("sess-1".into());
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["type"], "resume_session");
        assert_eq!(json["session_id"], "sess-1");
    }

    #[test]
    fn test_send_set_model_json_shape() {
        let mut request = ClientRequest::new("set_model");
        request.session_id = Some("sess-1".into());
        request.model = Some("claude-3.5".into());
        request.max_tokens = Some(8192);
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["type"], "set_model");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["model"], "claude-3.5");
        assert_eq!(json["max_tokens"], 8192);
    }

    #[test]
    fn test_send_compact_json_shape() {
        let mut request = ClientRequest::new("compact");
        request.session_id = Some("sess-1".into());
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["type"], "compact");
        assert_eq!(json["session_id"], "sess-1");
    }
}
