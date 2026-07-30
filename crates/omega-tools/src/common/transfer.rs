//! Transfer tool — sends a file from the agent's environment to the TUI.
//!
//! The LLM uses this tool to transfer any file (patch, result, artifact) to
//! the user's local machine. The tool reads the file from the agent-accessible
//! path and returns its content; the TUI (omega-tui) intercepts the result
//! and writes it to the user's PWD.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::super::tool::Tool;
use omega_core::core::ToolRuntime;
use omega_core::core::{ToolInfo, ToolResult};
use omega_llm::ToolDefinition;

/// Input for the Transfer tool
#[derive(Debug, Deserialize)]
struct TransferInput {
    /// Path to the file on the agent's filesystem to transfer
    file_path: String,
}

/// Transfer tool — sends a file from the agent to the user's machine.
///
/// The LLM calls this tool with a file path. The tool reads the file and
/// returns its content. When the TUI receives a `ToolEnd` for this tool
/// it writes the content to the user's PWD with a session-timestamped name.
pub struct TransferTool;

impl TransferTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TransferTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TransferTool {
    fn name(&self) -> &str {
        "Transfer"
    }

    fn description(&self) -> &str {
        "Transfer a file from the agent's filesystem to the user's local machine."
    }

    fn definition(&self) -> ToolDefinition {
        crate::def_to_tool_definition(&omega_tool_defs::transfer::DEF)
    }

    fn get_info(&self, input: &Value) -> ToolInfo {
        let file_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");

        ToolInfo {
            name: "Transfer".to_string(),
            action_description: "Transfer a file to the user's local machine".to_string(),
            details: Some(format!("File: {}", file_path)),
        }
    }

    async fn execute(&self, input: &Value, _rt: &mut dyn ToolRuntime) -> Result<ToolResult> {
        let transfer_input: TransferInput = serde_json::from_value(input.clone())
            .map_err(|e| anyhow::anyhow!("Invalid Transfer input: {}", e))?;

        let file_path = &transfer_input.file_path;

        if file_path.is_empty() {
            return Ok(ToolResult::error("file_path is required"));
        }

        // Read the file from the agent's filesystem
        let content = match tokio::fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Failed to read file '{}': {}",
                    file_path, e
                )));
            }
        };

        if content.is_empty() {
            return Ok(ToolResult::error(format!("File '{}' is empty", file_path)));
        }

        let line_count = content.lines().count();
        let size_kb = content.len() / 1024;

        tracing::info!(
            "Transfer: file={}, {} lines, ~{} KB",
            file_path,
            line_count,
            size_kb
        );

        Ok(ToolResult::success(content))
    }
}
