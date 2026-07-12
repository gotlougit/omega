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
    /// An unrecognised variant (forward-compatibility).
    Unknown(Value),
}

/// A decoded output chunk from the agent daemon, mirroring the relevant
/// variants of `picrust::core::OutputChunk`.
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
        result: ToolResultWire,
    },
    AskUserQuestion {
        request_id: String,
        questions: Vec<UserQuestionWire>,
    },
    Status(String),
    Error(String),
    Done,
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

#[derive(Debug)]
pub struct UserQuestionWire {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOptionWire>,
    pub multi_select: bool,
}

#[derive(Debug)]
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
            for (key, inner) in map.iter() {
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
                        let id = inner.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = inner.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let input = inner.get("input").cloned().unwrap_or(Value::Null);
                        OutputChunk::ToolStart { id, name, input }
                    }
                    "ToolProgress" => {
                        let id = inner.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let output = inner.get("output").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        OutputChunk::ToolProgress { id, output }
                    }
                    "ToolEnd" => {
                        let id = inner.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let result = parse_tool_result(inner.get("result"))
                            .unwrap_or_else(|| ToolResultWire {
                                text: String::new(),
                                is_error: false,
                                content: None,
                            });
                        OutputChunk::ToolEnd { id, result }
                    }
                    "AskUserQuestion" => {
                        let request_id = inner.get("request_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let questions = inner
                            .get("questions")
                            .and_then(|a| a.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(parse_user_question)
                                    .collect()
                            })
                            .unwrap_or_default();
                        OutputChunk::AskUserQuestion { request_id, questions }
                    }
                    "Status" => inner
                        .as_str()
                        .map(|s| OutputChunk::Status(s.to_string()))
                        .unwrap_or(OutputChunk::Unknown),
                    "Error" => inner
                        .as_str()
                        .map(|s| OutputChunk::Error(s.to_string()))
                        .unwrap_or(OutputChunk::Unknown),
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
    let is_error = val.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);

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
        multi_select: val.get("multi_select").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

impl ServerEvent {
    /// Parse a line of JSON received from the daemon.
    pub fn from_json_line(line: &str) -> Result<Self> {
        let val: Value = serde_json::from_str(line)?;
        let obj = val.as_object().ok_or_else(|| {
            anyhow::anyhow!("Server event is not a JSON object")
        })?;
        let type_name = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
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
            _ => Ok(ServerEvent::Unknown(val)),
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Session configuration sent with a `run` request.
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

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let json = serde_json::to_string(value)?;
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }
}
