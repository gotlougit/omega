//! Tool registry for managing available tools

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use super::tool::Tool;
use omega_core::core::{ToolInfo, ToolResult, ToolRuntime};
use omega_llm::ToolDefinition;

/// Registry that holds all available tools
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create a new empty tool registry
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool in the registry
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let name = tool.name().to_string();
        tracing::info!("Registering tool: {}", name);
        self.tools.insert(name, Arc::new(tool));
    }

    /// Get a tool by name
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Get all tool definitions
    pub fn get_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// Get information about a tool invocation
    pub fn get_tool_info(&self, name: &str, input: &Value) -> Option<ToolInfo> {
        self.tools.get(name).map(|t| t.get_info(input))
    }

    /// Execute a tool by name
    pub async fn execute(
        &self,
        name: &str,
        input: &Value,
        rt: &mut dyn ToolRuntime,
    ) -> Result<ToolResult> {
        let tool = self
            .tools
            .get(name)
            .with_context(|| format!("Tool not found: {}", name))?;

        tracing::info!("Executing tool: {}", name);
        tracing::debug!("Input: {:?}", input);

        let result = tool.execute(input, rt).await?;

        tracing::debug!("Tool {} completed. Is error: {}", name, result.is_error);

        Ok(result)
    }

    /// Get the list of tool names
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Get the number of registered tools
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for ToolRegistry {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_registry() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.get("nonexistent").is_none());
    }
}
