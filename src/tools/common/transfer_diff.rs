//! Transfer tool — sends a file from the agent's environment to the TUI.
//!
//! The LLM uses this tool to transfer any file (patch, result, artifact) to
//! the user's local machine. The tool reads the file from the agent-accessible
//! path and returns its content; the TUI (omega-tui) intercepts the result
//! and writes it to the user's PWD.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::super::tool::{Tool, ToolInfo, ToolResult};
use crate::llm::{ToolDefinition, ToolInputSchema};
use crate::runtime::AgentInternals;

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
        use crate::llm::types::CustomTool;

        ToolDefinition::Custom(CustomTool {
            name: "Transfer".to_string(),
            description: Some(
                "Use this tool to transfer a file from the agent's environment to the user.\n\
                 The file content will be saved as a file on the user's local machine.\n\n\
                 Typical usage:\n\
                 1. Generate or create a file (e.g. a patch, result, or artifact)\n\
                 2. Pass the absolute path to the file as `file_path`\n\n\
                 The file will be saved with a name like `<original-name>-<session>-<timestamp>`."
                    .to_string(),
            ),
            input_schema: ToolInputSchema {
                schema_type: "object".to_string(),
                properties: Some(json!({
                    "file_path": {
                        "type": "string",
                        "description": "Absolute path to the file on the agent's filesystem to transfer"
                    }
                })),
                required: Some(vec!["file_path".to_string()]),
            },
            tool_type: None,
            cache_control: None,
        })
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

    async fn execute(&self, input: &Value, _internals: &mut AgentInternals) -> Result<ToolResult> {
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
            return Ok(ToolResult::error(format!(
                "File '{}' is empty",
                file_path
            )));
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

    fn requires_permission(&self) -> bool {
        false // The transfer IS the user interaction — no additional permission needed
    }
}
