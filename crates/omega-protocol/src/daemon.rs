//! Protocol between UI clients and `omega-loop`.

use omega_core::core::{OutputChunk, RoleInfo, SessionInfo, ToolResult, ToolResultData};
use omega_projects::{ActiveProject, ProjectInfo};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request accepted by `omega-loop`.
///
/// The protocol historically used one permissive object for every command.
/// Keeping that representation preserves compatibility with existing clients
/// while giving the daemon one canonical deserializer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientRequest {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<SessionConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ActiveProject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl ClientRequest {
    pub fn new(msg_type: impl Into<String>) -> Self {
        Self {
            msg_type: msg_type.into(),
            session_id: None,
            content: None,
            config: None,
            model: None,
            max_tokens: None,
            query: None,
            spec: None,
            project: None,
            role: None,
        }
    }
}

/// Per-turn configuration carried by `run` requests.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_stream")]
    pub stream: bool,
    #[serde(default)]
    pub think: bool,
    #[serde(default)]
    pub no_cache: bool,
}

const fn default_stream() -> bool {
    true
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

/// A core output chunk at the daemon wire boundary.
///
/// The `Value` fallback deliberately accepts chunks introduced by a newer
/// daemon, allowing an older client to keep reading subsequent events.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WireChunk {
    Known(OutputChunk),
    Unknown(Value),
}

impl WireChunk {
    pub fn as_known(&self) -> Option<&OutputChunk> {
        match self {
            Self::Known(chunk) => Some(chunk),
            Self::Unknown(_) => None,
        }
    }

    pub fn into_known(self) -> Option<OutputChunk> {
        match self {
            Self::Known(chunk) => Some(chunk),
            Self::Unknown(_) => None,
        }
    }
}

impl From<OutputChunk> for WireChunk {
    fn from(chunk: OutputChunk) -> Self {
        Self::Known(chunk)
    }
}

/// Human-readable representation of a core tool result for UI clients.
pub fn tool_result_text(result: &ToolResult) -> String {
    match &result.content {
        ToolResultData::Text(text) => text.clone(),
        ToolResultData::Image { .. } => "[Image]".to_string(),
        ToolResultData::Document { description, .. } => {
            format!("[Document: {description}]")
        }
    }
}

/// An event emitted by `omega-loop`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    Created {
        session_id: String,
        session_name: String,
    },
    Chunk {
        session_id: String,
        chunk: WireChunk,
    },
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    SessionResumed {
        session_id: String,
        session_name: String,
    },
    HistoryMessage {
        session_id: String,
        role: String,
        content: String,
    },
    ModelChanged {
        /// Older daemons omitted this field; retain read compatibility.
        #[serde(default)]
        session_id: String,
        model: String,
    },
    SessionCompacted {
        session_id: String,
    },
    ModelList {
        models: Vec<String>,
    },
    ProjectList {
        projects: Vec<ProjectInfo>,
    },
    RoleList {
        roles: Vec<RoleInfo>,
    },
    ProjectActive {
        project: ProjectInfo,
        worktree_path: String,
        branch: String,
    },
    SystemMsg {
        message: String,
    },
    /// A newer top-level event. Unknown events are ignored, not fatal.
    #[serde(other)]
    Unknown,
}

impl ServerEvent {
    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_keep_existing_event_shapes() {
        let fixtures = [
            r#"{"type":"Created","session_id":"s1","session_name":"one"}"#,
            r#"{"type":"ModelChanged","session_id":"s1","model":"gpt-test"}"#,
            r#"{"type":"SessionCompacted","session_id":"s1"}"#,
            r#"{"type":"SystemMsg","message":"hello"}"#,
            r#"{"type":"Chunk","session_id":"s1","chunk":{"TextDelta":"hi"}}"#,
            r#"{"type":"Chunk","session_id":"s1","chunk":"Done"}"#,
            r#"{"type":"Chunk","session_id":"s1","chunk":{"ToolEnd":{"id":"t1","name":"Read","input":{"file_path":"a"},"result":{"content":{"Text":"ok"},"is_error":false}}}}"#,
            r#"{"type":"Chunk","session_id":"s1","chunk":{"ToolEnd":{"id":"t2","name":"Image","input":{},"result":{"content":{"Image":{"data":[1,2],"media_type":"image/png"}},"is_error":false}}}}"#,
            r#"{"type":"Chunk","session_id":"s1","chunk":{"ToolEnd":{"id":"t3","name":"Document","input":{},"result":{"content":{"Document":{"data":[3],"media_type":"application/pdf","description":"manual"}},"is_error":true}}}}"#,
            r#"{"type":"Chunk","session_id":"s1","chunk":{"CacheTelemetry":{"input_tokens":100,"output_tokens":25,"cache_read_tokens":80,"cache_creation_tokens":10}}}"#,
        ];

        for fixture in fixtures {
            let event: ServerEvent = serde_json::from_str(fixture).unwrap();
            let actual = serde_json::to_value(event).unwrap();
            let expected: Value = serde_json::from_str(fixture).unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn old_model_changed_without_session_id_still_decodes() {
        let event: ServerEvent =
            serde_json::from_str(r#"{"type":"ModelChanged","model":"old"}"#).unwrap();
        assert!(matches!(
            event,
            ServerEvent::ModelChanged { session_id, model }
                if session_id.is_empty() && model == "old"
        ));
    }

    #[test]
    fn unknown_top_level_event_is_non_fatal() {
        let event: ServerEvent =
            serde_json::from_str(r#"{"type":"FutureEvent","payload":42}"#).unwrap();
        assert!(matches!(event, ServerEvent::Unknown));
    }

    #[test]
    fn unknown_output_chunk_is_non_fatal() {
        let event: ServerEvent = serde_json::from_str(
            r#"{"type":"Chunk","session_id":"s1","chunk":{"FutureChunk":{"x":1}}}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            ServerEvent::Chunk {
                chunk: WireChunk::Unknown(_),
                ..
            }
        ));
    }

    #[test]
    fn absent_request_fields_are_omitted() {
        let request = ClientRequest::new("list_models");
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({"type":"list_models"})
        );
    }

    #[test]
    fn tool_result_display_covers_every_content_kind() {
        assert_eq!(tool_result_text(&ToolResult::success("text")), "text");
        assert_eq!(
            tool_result_text(&ToolResult::image(vec![1], "image/png")),
            "[Image]"
        );
        assert_eq!(
            tool_result_text(&ToolResult::document(vec![1], "application/pdf", "manual")),
            "[Document: manual]"
        );
    }
}
