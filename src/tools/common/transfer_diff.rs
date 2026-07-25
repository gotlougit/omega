//! TransferDiff tool — sends a patch/diff to the TUI for the user to save.
//!
//! The LLM generates or captures a diff (e.g. via `git diff`) and passes the
//! content to this tool.  The tool returns the patch as its result text; the
//! TUI (omega-tui) intercepts the result and writes it to the user's PWD.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::super::tool::{Tool, ToolInfo, ToolResult};
use crate::llm::{ToolDefinition, ToolInputSchema};
use crate::runtime::AgentInternals;

/// Input for the TransferDiff tool
#[derive(Debug, Deserialize)]
struct TransferDiffInput {
    /// The patch / diff content to transfer
    patch: String,
    /// Optional hint for the file name (without extension)
    file_name: Option<String>,
}

/// TransferDiff tool — sends a patch/diff to the user's machine.
///
/// The LLM calls this tool with the patch content.  The tool returns the
/// patch as its result text.  When the TUI receives a `ToolEnd` for this
/// tool it writes the patch to `./transfer-{session}-{timestamp}.patch`.
pub struct TransferDiffTool;

impl TransferDiffTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TransferDiffTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TransferDiffTool {
    fn name(&self) -> &str {
        "TransferDiff"
    }

    fn description(&self) -> &str {
        "Transfer a patch or diff to the user's local machine. The patch is saved as a file on the user's filesystem."
    }

    fn definition(&self) -> ToolDefinition {
        use crate::llm::types::CustomTool;

        ToolDefinition::Custom(CustomTool {
            name: "TransferDiff".to_string(),
            description: Some(
                "Use this tool to transfer a patch, diff, or code change to the user.\n\
                 The patch content will be saved as a file on the user's local machine.\n\n\
                 Typical usage:\n\
                 1. Generate a diff with `git diff` or `git format-patch`\n\
                 2. Pass the full diff output as the `patch` parameter\n\
                 3. Optionally provide a `file_name` hint (without extension)\n\n\
                 The file will be saved with a name like `transfer-<session>-<timestamp>.patch`."
                    .to_string(),
            ),
            input_schema: ToolInputSchema {
                schema_type: "object".to_string(),
                properties: Some(json!({
                    "patch": {
                        "type": "string",
                        "description": "The full patch or diff content to transfer"
                    },
                    "file_name": {
                        "type": "string",
                        "description": "Optional hint for the base file name (without extension). Defaults to 'patch'."
                    }
                })),
                required: Some(vec!["patch".to_string()]),
            },
            tool_type: None,
            cache_control: None,
        })
    }

    fn get_info(&self, input: &Value) -> ToolInfo {
        let patch_preview = input
            .get("patch")
            .and_then(|v| v.as_str())
            .map(|s| {
                let preview: String = s.lines().take(5).collect::<Vec<_>>().join("\n");
                if s.len() > preview.len() {
                    format!("{}...", preview)
                } else {
                    preview
                }
            })
            .unwrap_or_else(|| "<empty>".to_string());

        ToolInfo {
            name: "TransferDiff".to_string(),
            action_description: "Transfer a patch/diff to the user's local machine".to_string(),
            details: Some(format!("Patch preview:\n{}", patch_preview)),
        }
    }

    async fn execute(&self, input: &Value, _internals: &mut AgentInternals) -> Result<ToolResult> {
        let transfer_input: TransferDiffInput = serde_json::from_value(input.clone())
            .map_err(|e| anyhow::anyhow!("Invalid TransferDiff input: {}", e))?;

        let patch = transfer_input.patch;
        let file_name = transfer_input
            .file_name
            .unwrap_or_else(|| "patch".to_string());

        // Validate patch is non-empty
        if patch.trim().is_empty() {
            return Ok(ToolResult::error("Patch content is empty"));
        }

        let line_count = patch.lines().count();
        let size_kb = patch.len() / 1024;

        tracing::info!(
            "TransferDiff: {} lines, ~{} KB, file_name_hint={}",
            line_count,
            size_kb,
            file_name
        );

        Ok(ToolResult::success(patch))
    }

    fn requires_permission(&self) -> bool {
        false // The transfer IS the user interaction — no additional permission needed
    }
}
