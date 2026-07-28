//! Agent Configuration

use std::sync::Arc;

use crate::helpers::InjectionChain;
use omega_llm::ThinkingConfig;
use omega_tools::ToolRegistry;

/// Configuration for a StandardAgent
pub struct AgentConfig {
    /// Tool registry (optional - agent can work without tools)
    pub tools: Option<Arc<ToolRegistry>>,

    /// Context injection chain (applied before each LLM call)
    pub injections: InjectionChain,

    /// Maximum number of tool iterations per turn
    pub max_tool_iterations: usize,

    /// Whether to enable debug logging (API calls, tool calls)
    pub debug_enabled: bool,

    /// Whether to enable streaming responses from the LLM
    pub streaming_enabled: bool,

    /// Whether to enable prompt caching
    pub enable_prompt_caching: bool,

    /// Whether to auto-save session after each turn
    pub auto_save_session: bool,

    /// Turn retry configuration
    pub turn_retry: TurnRetryConfig,

    /// Extended thinking configuration
    pub thinking: Option<ThinkingConfig>,
}

/// Configuration for automatic turn retries on transient errors.
#[derive(Debug, Clone)]
pub struct TurnRetryConfig {
    /// Whether retry is enabled. Default: true
    pub enabled: bool,
    /// Maximum number of retry attempts. Default: 3
    pub max_retries: u32,
    /// Seconds to wait between retry attempts. Default: 15
    pub retry_delay_secs: u64,
}

impl AgentConfig {
    pub fn new() -> Self {
        Self {
            tools: None,
            injections: InjectionChain::new(),
            max_tool_iterations: 100,
            auto_save_session: true,
            debug_enabled: false,
            streaming_enabled: false,
            enable_prompt_caching: true,
            turn_retry: TurnRetryConfig {
                enabled: true,
                max_retries: 3,
                retry_delay_secs: 15,
            },
            thinking: None,
        }
    }

    /// Set the tool registry
    pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = Some(tools);
        self
    }



    /// Enable or disable streaming responses
    pub fn with_streaming(mut self, enabled: bool) -> Self {
        self.streaming_enabled = enabled;
        self
    }

    /// Enable extended thinking with a token budget
    pub fn with_thinking(mut self, budget_tokens: u32) -> Self {
        self.thinking = Some(ThinkingConfig::enabled(budget_tokens));
        self
    }

    /// Enable or disable prompt caching
    pub fn with_prompt_caching(mut self, enabled: bool) -> Self {
        self.enable_prompt_caching = enabled;
        self
    }

    /// Get tool definitions (empty vec if no tools)
    pub fn tool_definitions(&self) -> Vec<omega_llm::ToolDefinition> {
        self.tools
            .as_ref()
            .map(|t| t.get_definitions())
            .unwrap_or_default()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("tools", &self.tools.as_ref().map(|t| t.tool_names()))
            .field("max_tool_iterations", &self.max_tool_iterations)
            .field("debug_enabled", &self.debug_enabled)
            .field("streaming_enabled", &self.streaming_enabled)
            .field("enable_prompt_caching", &self.enable_prompt_caching)
            .field("turn_retry", &self.turn_retry)
            .field("thinking", &self.thinking)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_config_defaults() {
        let config = AgentConfig::default();
        assert!(!config.debug_enabled);
    }
}
