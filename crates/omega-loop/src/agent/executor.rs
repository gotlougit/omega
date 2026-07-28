//! Tool Executor
//!
//! Executes tools with debug logging.

use serde_json::Value;

use crate::helpers::Debugger;
use crate::runtime::AgentInternals;
use omega_core::core::ToolResult;
use omega_tools::ToolRegistry;

/// Handles tool execution.
pub struct ToolExecutor;

impl ToolExecutor {
    /// Execute a tool.
    pub async fn execute(
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

        // Send tool start notification
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

        // Send tool end notification
        internals.send_tool_end(tool_id, result.clone());
        internals.context.current_tool_use_id = None;

        result
    }
}
