//! Tool trait definition
//!
//! All tools implement this trait to provide a consistent interface.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use omega_core::core::{ToolInfo, ToolResult, ToolRuntime};
use omega_llm::ToolDefinition;

/// Trait for tools that the agent can use
///
/// All tools must implement this trait to be usable by the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the name of this tool
    fn name(&self) -> &str;

    /// Get a description of this tool
    fn description(&self) -> &str;

    /// Get the tool definition for the Anthropic API
    fn definition(&self) -> ToolDefinition;

    /// Get information about what this tool invocation will do
    fn get_info(&self, input: &Value) -> ToolInfo;

    /// Execute the tool with the given input
    ///
    /// The input is a JSON value that matches the tool's input schema.
    /// `rt` provides access to the agent runtime (output channels, user questions, etc.).
    async fn execute(&self, input: &Value, rt: &mut dyn ToolRuntime) -> Result<ToolResult>;
}
