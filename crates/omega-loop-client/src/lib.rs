//! # omega-loop client — connect to the agent daemon from a UI process
//!
//! Provides `AgentdClient` which speaks the newline-delimited JSON protocol
//! to a running `omega-loop` daemon.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// ---------------------------------------------------------------------------
// Wire types (deserialization of server → client events)
// ---------------------------------------------------------------------------

/// An event received from the agent daemon.
#[derive(Debug)]
pub enum ServerEvent {
    /// A new session was created.
    Created {
        session_id: String,
        session_name: String,
    },
    /// A chunk of streaming output from a session.
    Chunk {
        session_id: String,
        chunk: OutputChunk,
    },
    /// List of available sessions (response to list_sessions).
    SessionList { sessions: Vec<String> },
    /// A session was resumed.
    SessionResumed {
        session_id: String,
        session_name: String,
    },
    /// Model was changed.
    ModelChanged { model: String },
    /// Session was compacted.
    SessionCompacted { session_id: String },
    /// A list of available models from the daemon.
    ModelList { models: Vec<String> },
    /// A system message from the daemon.
    SystemMsg(String),
    /// An unrecognised variant (forward-compatibility).
    Unknown(Value),
}

/// A decoded output chunk from the agent daemon, mirroring the relevant
/// variants of `omega::core::OutputChunk`.
#[derive(Debug)]
pub enum OutputChunk {
    TextDelta(String),
    TextComplete(String),
    ThinkingDelta(String),
    ThinkingComplete(String),
    ToolStart {
        id: String,
        name: String,
        input: Value,
    },
    ToolProgress {
        id: String,
        output: String,
    },
    ToolEnd {
        id: String,
        name: String,
        input: Value,
        result: ToolResultWire,
    },
    AskUserQuestion {
        request_id: String,
        questions: Vec<UserQuestionWire>,
    },
    PermissionRequest {
        tool_name: String,
        action: String,
        input: String,
        details: Option<String>,
    },
    Status(String),
    Error(String),
    Done,
    /// Prompt caching telemetry from the last LLM call
    CacheTelemetry {
        input_tokens: u32,
        cache_read_tokens: u32,
        cache_creation_tokens: u32,
    },
    Unknown,
}

#[derive(Debug)]
pub struct ToolResultWire {
    pub text: String,
    pub is_error: bool,
    pub content: Option<Vec<ContentBlockWire>>,
}

#[derive(Debug)]
pub struct ContentBlockWire {
    pub block_type: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UserQuestionWire {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOptionWire>,
    pub multi_select: bool,
}

#[derive(Debug, Clone)]
pub struct QuestionOptionWire {
    pub label: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// Custom JSON deserialization
// ---------------------------------------------------------------------------

fn parse_chunk(val: &Value) -> OutputChunk {
    match val {
        Value::String(s) if s == "Done" => OutputChunk::Done,
        Value::Object(map) => {
            // Externally tagged serde representation uses the variant name as key.
            // Find the first key that matches a known variant.
            if let Some((key, inner)) = map.iter().next() {
                return match key.as_str() {
                    "TextDelta" => inner
                        .as_str()
                        .map(|s| OutputChunk::TextDelta(s.to_string()))
                        .unwrap_or(OutputChunk::Unknown),
                    "TextComplete" => inner
                        .as_str()
                        .map(|s| OutputChunk::TextComplete(s.to_string()))
                        .unwrap_or(OutputChunk::Unknown),
                    "ThinkingDelta" => inner
                        .as_str()
                        .map(|s| OutputChunk::ThinkingDelta(s.to_string()))
                        .unwrap_or(OutputChunk::Unknown),
                    "ThinkingComplete" => inner
                        .as_str()
                        .map(|s| OutputChunk::ThinkingComplete(s.to_string()))
                        .unwrap_or(OutputChunk::Unknown),
                    "ToolStart" => {
                        let id = inner
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = inner
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = inner.get("input").cloned().unwrap_or(Value::Null);
                        OutputChunk::ToolStart { id, name, input }
                    }
                    "ToolProgress" => {
                        let id = inner
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let output = inner
                            .get("output")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        OutputChunk::ToolProgress { id, output }
                    }
                    "ToolEnd" => {
                        let id = inner
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = inner
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = inner.get("input").cloned().unwrap_or(Value::Null);
                        let result = parse_tool_result(inner.get("result")).unwrap_or_else(|| {
                            ToolResultWire {
                                text: String::new(),
                                is_error: false,
                                content: None,
                            }
                        });
                        OutputChunk::ToolEnd {
                            id,
                            name,
                            input,
                            result,
                        }
                    }
                    "AskUserQuestion" => {
                        let request_id = inner
                            .get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let questions = inner
                            .get("questions")
                            .and_then(|a| a.as_array())
                            .map(|arr| arr.iter().filter_map(parse_user_question).collect())
                            .unwrap_or_default();
                        OutputChunk::AskUserQuestion {
                            request_id,
                            questions,
                        }
                    }
                    "PermissionRequest" => {
                        let tool_name = inner
                            .get("tool_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let action = inner
                            .get("action")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = inner
                            .get("input")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let details = inner
                            .get("details")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        OutputChunk::PermissionRequest {
                            tool_name,
                            action,
                            input,
                            details,
                        }
                    }
                    "Status" => inner
                        .as_str()
                        .map(|s| OutputChunk::Status(s.to_string()))
                        .unwrap_or(OutputChunk::Unknown),
                    "Error" => inner
                        .as_str()
                        .map(|s| OutputChunk::Error(s.to_string()))
                        .unwrap_or(OutputChunk::Unknown),
                    "CacheTelemetry" => {
                        let input_tokens = inner
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        let cache_read_tokens = inner
                            .get("cache_read_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        let cache_creation_tokens = inner
                            .get("cache_creation_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                            as u32;
                        OutputChunk::CacheTelemetry {
                            input_tokens,
                            cache_read_tokens,
                            cache_creation_tokens,
                        }
                    }
                    "StateChange" => OutputChunk::Unknown,
                    _ => OutputChunk::Unknown,
                };
            }
            OutputChunk::Unknown
        }
        _ => OutputChunk::Unknown,
    }
}

fn parse_tool_result(val: Option<&Value>) -> Option<ToolResultWire> {
    let val = val?;
    let is_error = val
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Extract text from the serde externally-tagged ToolResultData::Text variant.
    let text = val
        .get("content")
        .and_then(|c| c.as_object())
        .and_then(|obj| {
            if let Some(text_val) = obj.get("Text") {
                text_val.as_str().map(|s| s.to_string())
            } else if let Some(_img) = obj.get("Image") {
                Some("[Image]".to_string())
            } else if let Some(doc) = obj.get("Document") {
                doc.get("description")
                    .and_then(|d| d.as_str())
                    .map(|d| format!("[Document: {d}]"))
            } else {
                None
            }
        })
        .unwrap_or_default();

    Some(ToolResultWire {
        text,
        is_error,
        content: None,
    })
}

fn parse_user_question(val: &Value) -> Option<UserQuestionWire> {
    Some(UserQuestionWire {
        question: val.get("question")?.as_str()?.to_string(),
        header: val.get("header")?.as_str()?.to_string(),
        options: val
            .get("options")?
            .as_array()?
            .iter()
            .filter_map(|o| {
                Some(QuestionOptionWire {
                    label: o.get("label")?.as_str()?.to_string(),
                    description: o.get("description")?.as_str()?.to_string(),
                })
            })
            .collect(),
        multi_select: val
            .get("multi_select")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

impl ServerEvent {
    /// Parse a line of JSON received from the daemon.
    pub fn from_json_line(line: &str) -> Result<Self> {
        let val: Value = serde_json::from_str(line)?;
        let obj = val
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Server event is not a JSON object"))?;
        let type_name = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match type_name {
            "Created" => Ok(ServerEvent::Created {
                session_id: obj
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                session_name: obj
                    .get("session_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
            "Chunk" => {
                let session_id = obj
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let chunk_val = obj.get("chunk").cloned().unwrap_or(Value::Null);
                let chunk = parse_chunk(&chunk_val);
                Ok(ServerEvent::Chunk { session_id, chunk })
            }
            "SessionList" => {
                let sessions = obj
                    .get("sessions")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(ServerEvent::SessionList { sessions })
            }
            "SessionResumed" => Ok(ServerEvent::SessionResumed {
                session_id: obj
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                session_name: obj
                    .get("session_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
            "ModelChanged" => Ok(ServerEvent::ModelChanged {
                model: obj
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
            "SessionCompacted" => Ok(ServerEvent::SessionCompacted {
                session_id: obj
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
            "ModelList" => Ok(ServerEvent::ModelList {
                models: obj
                    .get("models")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            "SystemMsg" => Ok(ServerEvent::SystemMsg(
                obj.get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            )),
            _ => Ok(ServerEvent::Unknown(val)),
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Session configuration sent with a `run` request.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub stream: bool,
    pub think: bool,
    pub no_cache: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            stream: true,
            think: false,
            no_cache: false,
        }
    }
}

/// Read half of a daemon connection — reads events from the socket.
pub struct DaemonReader {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
}

/// Write half of a daemon connection — sends commands to the socket.
pub struct DaemonWriter {
    writer: tokio::io::WriteHalf<UnixStream>,
}

/// A client connected to the omega-loop daemon.
pub struct AgentdClient {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
    writer: tokio::io::WriteHalf<UnixStream>,
}

impl AgentdClient {
    /// Connect to the omega-loop daemon using `OMEGA_LOOP_SOCKET_PATH` env or default.
    pub async fn connect() -> Result<Self> {
        let path = std::env::var("OMEGA_LOOP_SOCKET_PATH")
            .unwrap_or_else(|_| "/tmp/omega-loop.sock".to_string());
        Self::connect_to(&path).await
    }

    /// Connect to the omega-loop daemon at a specific socket path.
    pub async fn connect_to(path: &str) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("Cannot connect to omega-loop at {path}"))?;
        let (reader, writer) = tokio::io::split(stream);
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    /// Send a `run` request (creates session if new, then sends the message).
    pub async fn send_run(
        &mut self,
        session_id: &str,
        content: &str,
        config: &SessionConfig,
    ) -> Result<()> {
        let req = serde_json::json!({
            "type": "run",
            "session_id": session_id,
            "content": content,
            "config": {
                "stream": config.stream,
                "think": config.think,
                "no_cache": config.no_cache,
            },
        });
        self.write_json(&req).await
    }

    /// Send an `ask_response` to the daemon.
    pub async fn send_ask_response(
        &mut self,
        session_id: &str,
        request_id: &str,
        answers: HashMap<String, String>,
    ) -> Result<()> {
        let req = serde_json::json!({
            "type": "ask_response",
            "session_id": session_id,
            "request_id": request_id,
            "answers": answers,
        });
        self.write_json(&req).await
    }

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

    /// Try to read an event without blocking. Returns `None` if no data is available.
    pub fn try_recv_event(&mut self) -> Result<Option<ServerEvent>> {
        Ok(None)
    }

    /// Split the client into separate reader and writer halves.
    ///
    /// Use this when you need to read events and write commands
    /// concurrently from different tasks (no Mutex required).
    pub fn split(self) -> (DaemonReader, DaemonWriter) {
        (
            DaemonReader {
                reader: self.reader,
            },
            DaemonWriter {
                writer: self.writer,
            },
        )
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let json = serde_json::to_string(value)?;
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }
}

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

impl DaemonWriter {
    /// Send a `run` request (creates session if new, then sends the message).
    pub async fn send_run(
        &mut self,
        session_id: &str,
        content: &str,
        config: &SessionConfig,
    ) -> Result<()> {
        let req = serde_json::json!({
            "type": "run",
            "session_id": session_id,
            "content": content,
            "config": {
                "stream": config.stream,
                "think": config.think,
                "no_cache": config.no_cache,
            },
        });
        self.write_json(&req).await
    }

    /// Send an `ask_response` to the daemon.
    pub async fn send_ask_response(
        &mut self,
        session_id: &str,
        request_id: &str,
        answers: HashMap<String, String>,
    ) -> Result<()> {
        let req = serde_json::json!({
            "type": "ask_response",
            "session_id": session_id,
            "request_id": request_id,
            "answers": answers,
        });
        self.write_json(&req).await
    }

    /// Request the list of available sessions from the daemon.
    pub async fn send_list_sessions(&mut self) -> Result<()> {
        let req = serde_json::json!({
            "type": "list_sessions",
        });
        self.write_json(&req).await
    }

    /// Request the list of available models from the daemon.
    pub async fn send_list_models(&mut self) -> Result<()> {
        let req = serde_json::json!({
            "type": "list_models",
        });
        self.write_json(&req).await
    }

    /// Resume an existing session by ID.
    pub async fn send_resume_session(&mut self, session_id: &str) -> Result<()> {
        let req = serde_json::json!({
            "type": "resume_session",
            "session_id": session_id,
        });
        self.write_json(&req).await
    }

    /// Change the model for a session.
    pub async fn send_set_model(
        &mut self,
        session_id: &str,
        model: &str,
        max_tokens: u32,
    ) -> Result<()> {
        let req = serde_json::json!({
            "type": "set_model",
            "session_id": session_id,
            "model": model,
            "max_tokens": max_tokens,
        });
        self.write_json(&req).await
    }

    /// Compact the current session (summarize/trim history).
    pub async fn send_compact(&mut self, session_id: &str) -> Result<()> {
        let req = serde_json::json!({
            "type": "compact",
            "session_id": session_id,
        });
        self.write_json(&req).await
    }

    /// Interrupt the current LLM response.
    pub async fn send_interrupt(&mut self, session_id: &str) -> Result<()> {
        let req = serde_json::json!({
            "type": "interrupt",
            "session_id": session_id,
        });
        self.write_json(&req).await
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let json = serde_json::to_string(value)?;
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
        let json = r#"{"type":"SessionList","sessions":["sess1","sess2"]}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::SessionList { sessions } => {
                assert_eq!(sessions, vec!["sess1", "sess2"]);
            }
            _ => panic!("Expected SessionList event"),
        }
    }

    #[test]
    fn test_parse_model_changed_event() {
        let json = r#"{"type":"ModelChanged","model":"gpt-4o"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::ModelChanged { model } => {
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
            ServerEvent::SystemMsg(msg) => {
                assert_eq!(msg, "Hello world");
            }
            _ => panic!("Expected SystemMsg event"),
        }
    }

    #[test]
    fn test_parse_unknown_event() {
        let json = r#"{"type":"UnknownType","foo":"bar"}"#;
        let event = ServerEvent::from_json_line(json).unwrap();
        match event {
            ServerEvent::Unknown(val) => {
                assert_eq!(
                    val.get("type").and_then(|v| v.as_str()),
                    Some("UnknownType")
                );
            }
            _ => panic!("Expected Unknown event"),
        }
    }

    #[test]
    fn test_output_chunk_parse_text_delta() {
        let json = r#"{"TextDelta":"Hello"}"#;
        let val: serde_json::Value = serde_json::from_str(json).unwrap();
        let chunk = parse_chunk(&val);
        match chunk {
            OutputChunk::TextDelta(s) => assert_eq!(s, "Hello"),
            _ => panic!("Expected TextDelta"),
        }
    }

    #[test]
    fn test_output_chunk_parse_done() {
        let val = serde_json::Value::String("Done".to_string());
        let chunk = parse_chunk(&val);
        match chunk {
            OutputChunk::Done => {}
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
        let json = serde_json::json!({
            "type": "run",
            "session_id": "sess-1",
            "content": "Hello",
            "config": {
                "stream": true,
                "think": false,
                "no_cache": false,
            },
        });
        assert_eq!(json["type"], "run");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["content"], "Hello");
        assert_eq!(json["config"]["stream"], true);
        assert_eq!(json["config"]["think"], false);
    }

    #[test]
    fn test_send_list_sessions_json_shape() {
        let json = serde_json::json!({"type": "list_sessions"});
        assert_eq!(json["type"], "list_sessions");
    }

    #[test]
    fn test_send_resume_session_json_shape() {
        let json = serde_json::json!({
            "type": "resume_session",
            "session_id": "sess-1",
        });
        assert_eq!(json["type"], "resume_session");
        assert_eq!(json["session_id"], "sess-1");
    }

    #[test]
    fn test_send_set_model_json_shape() {
        let json = serde_json::json!({
            "type": "set_model",
            "session_id": "sess-1",
            "model": "claude-3.5",
            "max_tokens": 8192,
        });
        assert_eq!(json["type"], "set_model");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["model"], "claude-3.5");
        assert_eq!(json["max_tokens"], 8192);
    }

    #[test]
    fn test_send_compact_json_shape() {
        let json = serde_json::json!({
            "type": "compact",
            "session_id": "sess-1",
        });
        assert_eq!(json["type"], "compact");
        assert_eq!(json["session_id"], "sess-1");
    }
}
