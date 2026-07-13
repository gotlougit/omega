//! Tool Executor
//!
//! Executes tools with debug logging. No permission checks — all tools are always allowed.

use serde_json::Value;

use crate::helpers::Debugger;
use crate::runtime::AgentInternals;
use crate::tools::{ToolRegistry, ToolResult};

/// Handles tool execution. All tools are executed directly without permission checks.
pub struct ToolExecutor;

impl ToolExecutor {
    /// Execute a tool — no permission checks, no hooks, just runs it.
    pub async fn execute_with_permission(
        internals: &mut AgentInternals,
        tools: &ToolRegistry,
        _hooks: Option<&crate::hooks::HookRegistry>,
        tool_name: &str,
        tool_id: &str,
        input: &Value,
        _hook_short_circuit: bool,
    ) -> ToolResult {
        Self::execute_tool(internals, tools, tool_name, tool_id, input).await
    }

    /// Execute a tool with logging
    async fn execute_tool(
        internals: &mut AgentInternals,
        tools: &ToolRegistry,
        tool_name: &str,
        tool_id: &str,
        input: &Value,
    ) -> ToolResult {
        internals.context.current_tool_use_id = Some(tool_id.to_string());
        internals.set_executing_tool(tool_name, tool_id).await;

        // Log tool call if debugger is enabled
        if let Some(debugger) = internals.context.get_resource::<Debugger>() {
            if let Err(e) = debugger.log_tool_call(tool_name, tool_id, input) {
                tracing::warn!("[Executor] Failed to log tool call: {}", e);
            }
        }

        // Send tool start notification — use the actual LLM-assigned tool-use ID
        internals.send_tool_start(tool_id, tool_name, input.clone());

        // Execute
        let result = match tools.execute(tool_name, input, internals).await {
            Ok(result) => result,
            Err(e) => ToolResult::error(format!("Tool execution failed: {}", e)),
        };

        // Log tool result if debugger is enabled
        if let Some(debugger) = internals.context.get_resource::<Debugger>() {
            if let Err(e) = debugger.log_tool_result(tool_name, tool_id, &result) {
                tracing::warn!("[Executor] Failed to log tool result: {}", e);
            }
        }

        // Send tool end notification — use the actual LLM-assigned tool-use ID
        internals.send_tool_end(tool_id, result.clone());
        internals.context.current_tool_use_id = None;

        result
    }

    /// Execute a tool directly (backwards compatibility)
    pub async fn execute(
        internals: &mut AgentInternals,
        tools: &ToolRegistry,
        tool_name: &str,
        tool_id: &str,
        input: &Value,
    ) -> ToolResult {
        Self::execute_tool(internals, tools, tool_name, tool_id, input).await
    }
}
