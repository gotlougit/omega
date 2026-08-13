//! ToolRuntime trait — minimal interface that tools need from the agent runtime
//!
//! This is the only behavioral trait in omega-core. It allows tools in `omega-tools`
//! to interact with the agent (send output, ask user questions, check interruption)
//! without depending on the concrete `AgentInternals` type in `omega-loop`.

use async_trait::async_trait;

use crate::core::output::{OutputChunk, ToolResult};

/// Runtime context passed to every tool execution.
///
/// Tools use this to stream output, check for interruption, and interact
/// with the user. The concrete implementation lives in `omega-loop`'s
/// `AgentInternals`.
#[async_trait]
pub trait ToolRuntime: Send + Sync {
    /// Send an output chunk to subscribers
    fn send_output(&self, chunk: OutputChunk);

    /// Send a text delta
    fn send_text(&self, text: &str) {
        self.send_output(OutputChunk::TextDelta(text.to_string()));
    }

    /// Send a text complete block
    fn send_text_complete(&self, text: &str) {
        self.send_output(OutputChunk::TextComplete(text.to_string()));
    }

    /// Send a thinking delta
    fn send_thinking(&self, text: &str) {
        self.send_output(OutputChunk::ThinkingDelta(text.to_string()));
    }

    /// Send a thinking complete block
    fn send_thinking_complete(&self, text: &str) {
        self.send_output(OutputChunk::ThinkingComplete(text.to_string()));
    }

    /// Notify that a tool has started
    fn send_tool_start(&self, id: &str, name: &str, input: serde_json::Value) {
        self.send_output(OutputChunk::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
            input,
        });
    }

    /// Send tool progress
    fn send_tool_progress(&self, id: &str, output: &str) {
        self.send_output(OutputChunk::ToolProgress {
            id: id.to_string(),
            output: output.to_string(),
        });
    }

    /// Notify that a tool has ended
    fn send_tool_end(&self, id: &str, name: &str, input: &serde_json::Value, result: ToolResult) {
        self.send_output(OutputChunk::ToolEnd {
            id: id.to_string(),
            name: name.to_string(),
            input: input.clone(),
            result,
        });
    }

    /// Send a status message
    fn send_status(&self, text: &str) {
        self.send_output(OutputChunk::Status(text.to_string()));
    }

    /// Send an error message
    fn send_error(&self, text: &str) {
        self.send_output(OutputChunk::Error(text.to_string()));
    }

    /// Send Done signal
    fn send_done(&self) {
        self.send_output(OutputChunk::Done);
    }

    /// Check whether the agent has been interrupted (e.g. by a user interrupt signal)
    fn is_interrupted(&self) -> bool;

    /// Whether the session is interactive (has a user attached)
    fn is_interactive(&self) -> bool {
        true
    }
}
