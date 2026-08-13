//! Input and output message types for agent communication

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::state::AgentState;

/// Messages that can be sent TO an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputMessage {
    /// User input text
    UserInput(String),

    /// Result from an async tool execution
    ToolResult {
        /// ID of the tool use this result is for
        tool_use_id: String,
        /// The tool result
        result: ToolResult,
    },

    /// Subagent completed
    SubAgentComplete {
        /// Session ID of the completed subagent
        session_id: String,
        /// Final result/summary from subagent
        result: Option<String>,
    },

    /// Request graceful interrupt
    Interrupt,

    /// Request shutdown
    Shutdown,
}

/// Output chunks streamed FROM an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputChunk {
    // --- Text Streaming ---
    /// Incremental text output
    TextDelta(String),

    /// Complete text block
    TextComplete(String),

    // --- Thinking Streaming ---
    /// Incremental thinking output
    ThinkingDelta(String),

    /// Complete thinking block
    ThinkingComplete(String),

    // --- Tool Execution ---
    /// Tool execution starting
    ToolStart {
        /// Tool use ID
        id: String,
        /// Tool name
        name: String,
        /// Tool input
        input: Value,
    },

    /// Incremental tool output (for long-running tools)
    ToolProgress {
        /// Tool use ID
        id: String,
        /// Progress output
        output: String,
    },

    /// Tool execution completed
    ToolEnd {
        /// Tool use ID
        id: String,
        /// Tool name
        name: String,
        /// Tool input
        input: Value,
        /// Tool result
        result: ToolResult,
    },

    // --- Subagent Events ---
    /// Subagent was spawned
    SubAgentSpawned {
        /// Session ID of new subagent
        session_id: String,
        /// Type of the subagent
        agent_type: String,
    },

    /// Output from a subagent (forwarded)
    SubAgentOutput {
        /// Session ID of the subagent
        session_id: String,
        /// The output chunk from subagent
        chunk: Box<OutputChunk>,
    },

    /// Subagent completed
    SubAgentComplete {
        /// Session ID of completed subagent
        session_id: String,
        /// Final result/summary
        result: Option<String>,
    },

    // --- State & Status ---
    /// Agent state changed
    StateChange(AgentState),

    /// Status update (for progress indicators)
    Status(String),

    // --- Completion ---
    /// Error occurred
    Error(String),

    /// Agent completed this turn
    Done,

    // --- Prompt Caching Telemetry ---
    /// Prompt caching statistics for the most recent LLM call
    CacheTelemetry(CacheTelemetry),
}

/// Prompt caching statistics for a single LLM request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheTelemetry {
    /// Total input tokens sent in this request
    pub input_tokens: u32,
    /// Output tokens generated in this request
    #[serde(default)]
    pub output_tokens: u32,
    /// Tokens read from cache in this request (cache hit)
    pub cache_read_tokens: u32,
    /// Tokens written to cache in this request (cache creation)
    pub cache_creation_tokens: u32,
}

impl CacheTelemetry {
    /// Compute the cache hit rate (0.0 – 100.0) for this request
    pub fn hit_rate_pct(&self) -> f64 {
        if self.input_tokens == 0 {
            return 0.0;
        }
        (self.cache_read_tokens as f64 / self.input_tokens as f64) * 100.0
    }

    /// Check whether any caching happened in this request
    pub fn has_caching(&self) -> bool {
        self.cache_read_tokens > 0 || self.cache_creation_tokens > 0
    }
}

/// Content type for tool results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolResultData {
    /// Text content
    Text(String),
    /// Image content (raw bytes and media type)
    Image { data: Vec<u8>, media_type: String },
    /// Document content (raw bytes, media type, and description)
    Document {
        data: Vec<u8>,
        media_type: String,
        description: String,
    },
}

/// Result of executing a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// The content of the tool result
    pub content: ToolResultData,
    /// Whether the tool execution resulted in an error
    pub is_error: bool,
}

impl ToolResult {
    /// Create a successful tool result with text content
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            content: ToolResultData::Text(output.into()),
            is_error: false,
        }
    }

    /// Create an error tool result
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: ToolResultData::Text(message.into()),
            is_error: true,
        }
    }

    /// Create a successful image result
    pub fn image(data: Vec<u8>, media_type: impl Into<String>) -> Self {
        Self {
            content: ToolResultData::Image {
                data,
                media_type: media_type.into(),
            },
            is_error: false,
        }
    }

    /// Create a successful document result
    pub fn document(
        data: Vec<u8>,
        media_type: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            content: ToolResultData::Document {
                data,
                media_type: media_type.into(),
                description: description.into(),
            },
            is_error: false,
        }
    }
}

/// Information about a tool
#[derive(Debug, Clone)]
pub struct ToolInfo {
    /// Name of the tool
    pub name: String,
    /// Human-readable description of what this invocation will do
    pub action_description: String,
    /// Additional details about the action (e.g., command to run, file to edit)
    pub details: Option<String>,
}

impl OutputChunk {
    /// Create a text delta chunk
    pub fn text(text: impl Into<String>) -> Self {
        OutputChunk::TextDelta(text.into())
    }

    /// Create a thinking delta chunk
    pub fn thinking(text: impl Into<String>) -> Self {
        OutputChunk::ThinkingDelta(text.into())
    }

    /// Create a tool start chunk
    pub fn tool_start(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        OutputChunk::ToolStart {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    /// Create a tool end chunk
    pub fn tool_end(
        id: impl Into<String>,
        name: impl Into<String>,
        input: Value,
        result: ToolResult,
    ) -> Self {
        OutputChunk::ToolEnd {
            id: id.into(),
            name: name.into(),
            input,
            result,
        }
    }

    /// Create an error chunk
    pub fn error(msg: impl Into<String>) -> Self {
        OutputChunk::Error(msg.into())
    }

    /// Check if this is a terminal chunk
    pub fn is_terminal(&self) -> bool {
        matches!(self, OutputChunk::Done | OutputChunk::Error(_))
    }

    /// Check if this is a text-related chunk
    pub fn is_text(&self) -> bool {
        matches!(
            self,
            OutputChunk::TextDelta(_) | OutputChunk::TextComplete(_)
        )
    }

    /// Check if this is a thinking-related chunk
    pub fn is_thinking(&self) -> bool {
        matches!(
            self,
            OutputChunk::ThinkingDelta(_) | OutputChunk::ThinkingComplete(_)
        )
    }

    /// Check if this is a tool-related chunk
    pub fn is_tool(&self) -> bool {
        matches!(
            self,
            OutputChunk::ToolStart { .. }
                | OutputChunk::ToolProgress { .. }
                | OutputChunk::ToolEnd { .. }
        )
    }
}

impl InputMessage {
    /// Create a user input message
    pub fn user_input(text: impl Into<String>) -> Self {
        InputMessage::UserInput(text.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_chunk_checks() {
        assert!(OutputChunk::Done.is_terminal());
        assert!(OutputChunk::error("oops").is_terminal());
        assert!(!OutputChunk::text("hello").is_terminal());

        assert!(OutputChunk::text("hello").is_text());
        assert!(OutputChunk::TextComplete("hello".into()).is_text());
        assert!(!OutputChunk::Done.is_text());

        assert!(OutputChunk::thinking("hmm").is_thinking());
        assert!(!OutputChunk::text("hello").is_thinking());
    }

    #[test]
    fn test_input_message_creation() {
        let msg = InputMessage::user_input("hello");
        assert!(matches!(msg, InputMessage::UserInput(s) if s == "hello"));
    }

    // -----------------------------------------------------------------------
    // CacheTelemetry tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cache_telemetry_all_zero() {
        let t = CacheTelemetry {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        assert!(!t.has_caching());
        assert_eq!(t.hit_rate_pct(), 0.0);
    }

    #[test]
    fn test_cache_telemetry_full_hit() {
        let t = CacheTelemetry {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 800,
            cache_creation_tokens: 200,
        };
        assert!(t.has_caching());
        assert_eq!(t.hit_rate_pct(), 80.0);
    }

    #[test]
    fn test_cache_telemetry_no_hit() {
        // First request: all tokens are new (cache creation only)
        let t = CacheTelemetry {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 1000,
        };
        assert!(t.has_caching());
        assert_eq!(t.hit_rate_pct(), 0.0);
    }

    #[test]
    fn test_cache_telemetry_hit_rate_zero_division() {
        // Edge case: division by zero handled
        let t = CacheTelemetry {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 50,
            cache_creation_tokens: 10,
        };
        assert!(t.has_caching());
        assert_eq!(t.hit_rate_pct(), 0.0);
    }

    #[test]
    fn test_cache_telemetry_has_caching_true_when_only_read() {
        let t = CacheTelemetry {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 5,
            cache_creation_tokens: 0,
        };
        assert!(t.has_caching());
    }

    #[test]
    fn test_cache_telemetry_has_caching_true_when_only_creation() {
        let t = CacheTelemetry {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 5,
        };
        assert!(t.has_caching());
    }

    #[test]
    fn test_cache_telemetry_hit_rate_hundred_percent() {
        let t = CacheTelemetry {
            input_tokens: 500,
            output_tokens: 250,
            cache_read_tokens: 500,
            cache_creation_tokens: 0,
        };
        assert_eq!(t.hit_rate_pct(), 100.0);
    }

    #[test]
    fn test_cache_telemetry_hit_rate_with_creation_but_no_read() {
        // Typical first request: tokens written to cache, none read
        let t = CacheTelemetry {
            input_tokens: 2000,
            output_tokens: 1000,
            cache_read_tokens: 0,
            cache_creation_tokens: 2000,
        };
        assert_eq!(t.hit_rate_pct(), 0.0);
        assert!(t.has_caching());
    }

    #[test]
    fn test_cache_telemetry_serialization() {
        let t = CacheTelemetry {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 800,
            cache_creation_tokens: 200,
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["input_tokens"], 1000);
        assert_eq!(json["output_tokens"], 500);
        assert_eq!(json["cache_read_tokens"], 800);
        assert_eq!(json["cache_creation_tokens"], 200);
    }

    #[test]
    fn test_cache_telemetry_deserialization() {
        let json =
            r#"{"input_tokens": 500, "cache_read_tokens": 400, "cache_creation_tokens": 100}"#;
        let t: CacheTelemetry = serde_json::from_str(json).unwrap();
        assert_eq!(t.input_tokens, 500);
        assert_eq!(t.output_tokens, 0);
        assert_eq!(t.cache_read_tokens, 400);
        assert_eq!(t.cache_creation_tokens, 100);
    }
}
