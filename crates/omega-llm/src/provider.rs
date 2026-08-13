//! LLM Provider trait
//!
//! Abstracts the LLM interface so that different providers can be used
//! interchangeably with the StandardAgent.

use anyhow::Result;
use futures::stream::Stream;
use std::pin::Pin;
use std::sync::Arc;

use super::types::{
    Message, StreamEvent, SystemPrompt, ThinkingConfig, ToolChoice, ToolDefinition,
};

/// Trait for LLM providers that can be used with StandardAgent.
///
/// This trait abstracts the interface needed by the agent loop, allowing
/// different LLM backends to be used interchangeably.
///
/// All providers work with the same internal message types, handling
/// translation to their own wire format internally.
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stream a request with tools and system prompt.
    ///
    /// Returns an async stream of StreamEvent that yields events as they arrive.
    /// This is the primary method used by the agent loop for streaming requests.
    async fn stream_with_tools_and_system(
        &self,
        messages: Vec<Message>,
        system: Option<SystemPrompt>,
        tools: Vec<ToolDefinition>,
        tool_choice: Option<ToolChoice>,
        thinking: Option<ThinkingConfig>,
        session_id: Option<&str>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;

    /// List available models from the API.
    ///
    /// Returns a list of model IDs. The default implementation returns an
    /// empty vec with an error, since not all providers support listing models.
    async fn list_models(&self) -> Result<Vec<String>> {
        Err(anyhow::anyhow!(
            "listing models is not supported by this provider"
        ))
    }

    /// Get the current model name.
    fn model(&self) -> String;

    /// Get the provider name (e.g., "openai").
    fn provider_name(&self) -> &str;

    /// Create a lightweight variant of this provider with a different model and max tokens.
    /// `None` max_tokens means no limit.
    fn create_variant(&self, model: &str, max_tokens: Option<u32>) -> Arc<dyn LlmProvider>;
}
