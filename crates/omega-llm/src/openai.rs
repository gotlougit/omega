//! OpenAI Chat Completions API client
//!
//! Implements the `LlmProvider` trait for OpenAI's Chat Completions API,
//! handling translation between the internal message types and OpenAI's
//! wire format.
//!
//! # Authentication
//!
//! ```ignore
//! // From environment (OPENAI_API_KEY, OPENAI_MODEL, OPENAI_BASE_URL)
//! let llm = OpenAIProvider::from_env()?;
//!
//! // With explicit key
//! let llm = OpenAIProvider::new("sk-...")
//!     .with_model("gpt-4o");
//!
//! // With dynamic auth provider
//! let llm = OpenAIProvider::with_auth_provider(|| async {
//!     let jwt = refresh_token().await?;
//!     Ok(AuthConfig::with_base_url(jwt, "https://my-proxy.com/v1/chat/completions"))
//! });
//! ```

use anyhow::{Context, Result};
use futures::stream::Stream;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio_util::io::StreamReader;

use super::auth::{auth_provider, AuthConfig, AuthSource};
use super::provider::LlmProvider;
use super::types::{
    ContentBlock, ContentBlockDeltaEvent, ContentBlockStart, ContentBlockStartEvent,
    ContentBlockStopEvent, ContentDelta, DeltaUsage, Message, MessageContent, MessageDeltaData,
    MessageDeltaEvent, MessageStartData, MessageStartEvent, StopReason,
    StreamEvent, SystemPrompt, ThinkingConfig, ToolChoice, ToolDefinition, Usage,
};

const DEFAULT_API_URL: &str = "https://api.openai.com/v1/chat/completions";

// ============================================================================
// OpenAI Wire Types
// ============================================================================

/// A message in OpenAI's chat completions format
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum OpenAIMessage {
    /// System message
    System { role: String, content: String },
    /// User message
    User { role: String, content: String },
    /// Assistant message (may include tool_calls)
    Assistant {
        role: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<OpenAIToolCall>>,
    },
    /// Tool result message
    Tool {
        role: String,
        tool_call_id: String,
        content: String,
    },
}

/// A tool call in OpenAI format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAIFunctionCall,
}

/// Function call details in OpenAI format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String,
}

/// OpenAI tool definition
#[derive(Debug, Clone, Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OpenAIFunction,
}

/// OpenAI function definition
#[derive(Debug, Clone, Serialize)]
struct OpenAIFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
}

/// OpenAI Chat Completions request body
#[derive(Debug, Clone, Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// Per-session prompt cache key (supported by some gateways)
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    /// Prompt cache retention (supported by some gateways)
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<String>,
}

/// OpenAI usage info
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenAIUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAIPromptTokensDetails>,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAIPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

/// Chunk from a streaming response
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenAIStreamChunk {
    id: Option<String>,
    object: Option<String>,
    created: Option<u64>,
    model: Option<String>,
    choices: Vec<OpenAIStreamChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

/// A choice in a streaming chunk
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenAIStreamChoice {
    index: u32,
    delta: OpenAIStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// Delta in a streaming chunk
#[derive(Debug, Clone, Deserialize)]
struct OpenAIStreamDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIStreamToolCall>>,
}

/// A tool call in a streaming delta
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenAIStreamToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    #[serde(rename = "type")]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<OpenAIStreamFunction>,
}

/// Function call in a streaming delta
#[derive(Debug, Clone, Deserialize)]
struct OpenAIStreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

// ============================================================================
// OpenAIProvider
// ============================================================================

/// OpenAI LLM provider using the Chat Completions API
pub struct OpenAIProvider {
    client: Client,
    auth: AuthSource,
    model: String,
    max_tokens: u32,
}

impl OpenAIProvider {
    /// Create from environment variables
    ///
    /// Reads from:
    /// - `OPENAI_API_KEY` (required)
    /// - `OPENAI_MODEL` (optional, defaults to `gpt-4o`)
    /// - `OPENAI_BASE_URL` (optional, defaults to OpenAI API)
    /// - `OPENAI_MAX_TOKENS` (optional, defaults to 8192)
    pub fn from_env() -> Result<Self> {
        tracing::info!("Creating OpenAI provider from environment");

        let api_key =
            env::var("OPENAI_API_KEY").context("OPENAI_API_KEY environment variable not set")?;

        let base_url = env::var("OPENAI_BASE_URL").ok();
        let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
        let max_tokens = env::var("OPENAI_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8192);

        tracing::info!("Using model: {}", model);
        tracing::info!("Max tokens: {}", max_tokens);

        Ok(Self {
            client: Client::new(),
            auth: AuthSource::Static(AuthConfig { api_key, base_url }),
            model,
            max_tokens,
        })
    }

    /// Create with an explicit API key
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            auth: AuthSource::Static(AuthConfig::new(api_key)),
            model: String::new(),
            max_tokens: 8192,
        }
    }

    /// Create with a dynamic auth provider callback
    pub fn with_auth_provider<F, Fut>(provider: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<AuthConfig>> + Send + 'static,
    {
        Self {
            client: Client::new(),
            auth: AuthSource::Dynamic(Arc::new(auth_provider(provider))),
            model: String::new(),
            max_tokens: 8192,
        }
    }

    /// Set the model
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set max tokens
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Get current model
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Create a variant with a different model and max tokens, sharing auth
    pub fn with_model_and_tokens_override(
        &self,
        model: impl Into<String>,
        max_tokens: u32,
    ) -> Self {
        Self {
            client: Client::new(),
            auth: self.auth.clone(),
            model: model.into(),
            max_tokens,
        }
    }

    /// Stream a request and return SSE events
    async fn send_streaming_request(
        &self,
        mut request: OpenAIRequest,
        session_id: Option<&str>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // When a session_id is provided, use it as the prompt cache key
        // so that the cache is scoped per session. Some gateways
        // accept these extra fields; others silently ignore them.
        if let Some(sid) = session_id {
            request.prompt_cache_key = Some(sid.to_string());
            request.prompt_cache_retention = Some("24h".to_string());
        }

        let auth_config = self
            .auth
            .get_auth()
            .await
            .context("Failed to get authentication credentials")?;
        let api_url = auth_config.base_url.as_deref().unwrap_or(DEFAULT_API_URL);

        let mut req_body = serde_json::to_value(request)?;
        req_body["stream"] = json!(true);
        let body = req_body.to_string();

        let response = self
            .client
            .post(api_url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", auth_config.api_key))
            .body(body)
            .send()
            .await
            .context("Failed to send streaming request to OpenAI API")?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            anyhow::bail!("OpenAI API error ({}): {}", status, error_text);
        }

        let byte_stream = response.bytes_stream();
        let stream_reader = StreamReader::new(
            byte_stream.map(|r| r.map_err(|e| std::io::Error::other(e.to_string()))),
        );
        let buf_reader = tokio::io::BufReader::new(stream_reader);

        let stream = async_stream::try_stream! {
            let mut lines = buf_reader.lines();
            let mut full_content = String::new();
            let mut tool_calls: Vec<AccumulatedToolCall> = Vec::new();

            while let Some(line) = lines.next_line().await? {
                if !line.starts_with("data: ") {
                    continue;
                }

                let data = &line[6..];

                // OpenAI sends "data: [DONE]" to signal stream end
                if data == "[DONE]" {
                    yield StreamEvent::MessageStop;
                    break;
                }

                match serde_json::from_str::<OpenAIStreamChunk>(data) {
                    Ok(chunk) => {
                        // First chunk may come before any choice data
                        if chunk.choices.is_empty() {
                            continue;
                        }

                        for choice in &chunk.choices {
                            let delta = &choice.delta;

                            // Role appears on first chunk → emit MessageStart
                            if delta.role.as_deref() == Some("assistant") {
                                let usage = chunk.usage.as_ref().map(|u| Usage {
                                    input_tokens: u.prompt_tokens,
                                    output_tokens: u.completion_tokens,
                                    cache_creation_input_tokens: None,
                                    cache_read_input_tokens: u.prompt_tokens_details
                                        .as_ref()
                                        .map(|d| d.cached_tokens),
                                    thoughts_token_count: None,
                                });

                                yield StreamEvent::MessageStart(MessageStartEvent {
                                    message: MessageStartData {
                                        id: chunk.id.clone().unwrap_or_default(),
                                        message_type: "message".to_string(),
                                        role: "assistant".to_string(),
                                        content: vec![],
                                        model: chunk.model.clone().unwrap_or_default(),
                                        stop_reason: None,
                                        stop_sequence: None,
                                        usage: usage.unwrap_or(Usage {
                                            input_tokens: 0,
                                            output_tokens: 0,
                                            cache_creation_input_tokens: None,
                                            cache_read_input_tokens: None,
                                            thoughts_token_count: None,
                                        }),
                                    },
                                });
                            }

                            // Handle content delta
                            if let Some(content) = &delta.content {
                                if !content.is_empty() {
                                    let is_new_text = full_content.is_empty();
                                    if is_new_text {
                                        // Start of a text block
                                        yield StreamEvent::ContentBlockStart(
                                            ContentBlockStartEvent {
                                                index: 0,
                                                content_block: ContentBlockStart::Text {
                                                    text: content.clone(),
                                                },
                                            },
                                        );
                                    }
                                    yield StreamEvent::ContentBlockDelta(
                                        ContentBlockDeltaEvent {
                                            index: 0,
                                            delta: ContentDelta::TextDelta {
                                                text: content.clone(),
                                            },
                                        },
                                    );
                                    full_content.push_str(content);
                                }
                            }

                            // Handle tool call deltas
                            if let Some(tc_deltas) = &delta.tool_calls {
                                for tc_delta in tc_deltas {
                                    let idx = tc_delta.index as usize;

                                    // Ensure we have an accumulator for this index
                                    while tool_calls.len() <= idx {
                                        tool_calls.push(AccumulatedToolCall::default());
                                    }
                                    let acc = &mut tool_calls[idx];

                                    if let Some(id) = &tc_delta.id {
                                        acc.id = id.clone();
                                    }
                                    if let Some(name) = &tc_delta.function.as_ref().and_then(|f| f.name.as_ref()) {
                                        acc.name = name.to_string();
                                    }
                                    if let Some(args) = &tc_delta.function.as_ref().and_then(|f| f.arguments.as_ref()) {
                                        acc.arguments.push_str(args);
                                    }
                                }
                            }

                            // Handle finish reason
                            if let Some(ref reason) = choice.finish_reason {
                                // Close the text block if we accumulated text
                                if !full_content.is_empty() {
                                    yield StreamEvent::ContentBlockStop(
                                        ContentBlockStopEvent { index: 0 },
                                    );
                                }

                                // Emit tool call blocks if we have any
                                if !tool_calls.is_empty() {
                                    for acc in &tool_calls {
                                        if !acc.id.is_empty() && !acc.name.is_empty() {
                                            let input: Value = serde_json::from_str(&acc.arguments)
                                                .unwrap_or(json!({}));
                                            // Emit as content block start+delta+stop sequence
                                            yield StreamEvent::ContentBlockStart(
                                                ContentBlockStartEvent {
                                                    index: if full_content.is_empty() { 0 } else { 1 },
                                                    content_block: ContentBlockStart::ToolUse {
                                                        id: acc.id.clone(),
                                                        name: acc.name.clone(),
                                                        input: input.clone(),
                                                        signature: None,
                                                    },
                                                },
                                            );
                                            yield StreamEvent::ContentBlockStop(
                                                ContentBlockStopEvent { index: if full_content.is_empty() { 0 } else { 1 } },
                                            );
                                        }
                                    }
                                }

                                // Map OpenAI finish_reason to StopReason
                                let stop_reason = match reason.as_str() {
                                    "stop" => Some(StopReason::EndTurn),
                                    "length" => Some(StopReason::MaxTokens),
                                    "tool_calls" => Some(StopReason::ToolUse),
                                    "content_filter" => Some(StopReason::Refusal),
                                    _ => Some(StopReason::EndTurn),
                                };

                                let output_tokens = chunk.usage
                                    .as_ref()
                                    .map(|u| u.completion_tokens)
                                    .unwrap_or(0);

                                yield StreamEvent::MessageDelta(MessageDeltaEvent {
                                    delta: MessageDeltaData {
                                        stop_reason,
                                        stop_sequence: None,
                                    },
                                    usage: DeltaUsage { output_tokens },
                                });
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse streaming chunk: {} — data: {}", e, data);
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

/// Accumulated tool call during streaming
#[derive(Default)]
struct AccumulatedToolCall {
    id: String,
    name: String,
    arguments: String,
}

// ============================================================================
// LlmProvider trait implementation
// ============================================================================

#[async_trait::async_trait]
impl LlmProvider for OpenAIProvider {
    async fn stream_with_tools_and_system(
        &self,
        messages: Vec<Message>,
        system: Option<SystemPrompt>,
        tools: Vec<ToolDefinition>,
        tool_choice: Option<ToolChoice>,
        thinking: Option<ThinkingConfig>,
        _session_id: Option<&str>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        if thinking.is_some() {
            tracing::warn!("OpenAI Chat Completions does not support extended thinking; ignoring");
        }

        let openai_messages = convert_messages_to_openai(&messages, &system);
        let openai_tools = convert_tools_to_openai(&tools);
        let openai_tool_choice = convert_tool_choice_to_openai(&tool_choice);

        let request = OpenAIRequest {
            model: self.model.clone(),
            messages: openai_messages,
            tools: if openai_tools.is_empty() {
                None
            } else {
                Some(openai_tools)
            },
            tool_choice: openai_tool_choice,
            temperature: None,
            max_tokens: Some(self.max_tokens),
            stream: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
        };

        self.send_streaming_request(request, _session_id).await
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    fn provider_name(&self) -> &str {
        "openai"
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        // Derive the models endpoint URL from the chat completions URL.
        // The standard OpenAI models endpoint is at:
        //   https://api.openai.com/v1/models
        // while the chat completions endpoint is at:
        //   https://api.openai.com/v1/chat/completions
        let auth_config = self
            .auth
            .get_auth()
            .await
            .context("Failed to get authentication credentials")?;

        let base_url = auth_config.base_url.as_deref().unwrap_or(DEFAULT_API_URL);
        let models_url = derive_models_url(base_url);

        let response = self
            .client
            .get(&models_url)
            .header("Authorization", format!("Bearer {}", auth_config.api_key))
            .send()
            .await
            .with_context(|| format!("Failed to send request to {models_url}"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("Failed to read response body")?;

        if !status.is_success() {
            anyhow::bail!("Models API error ({}): {}", status, body);
        }

        let models_response: serde_json::Value = serde_json::from_str(&body)
            .context("Failed to parse models response")?;

        let model_ids = models_response["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(model_ids)
    }

    fn create_variant(&self, model: &str, max_tokens: u32) -> Arc<dyn LlmProvider> {
        Arc::new(self.with_model_and_tokens_override(model, max_tokens))
    }
}

// ============================================================================
// Conversion Functions
// ============================================================================

/// Convert internal messages and system prompt to OpenAI message array
fn convert_messages_to_openai(
    messages: &[Message],
    system: &Option<SystemPrompt>,
) -> Vec<OpenAIMessage> {
    let mut out = Vec::new();

    // System prompt as first message
    match system {
        Some(SystemPrompt::Text(s)) => {
            out.push(OpenAIMessage::System {
                role: "system".to_string(),
                content: s.clone(),
            });
        }
        Some(SystemPrompt::Blocks(blocks)) => {
            let text: String = blocks.iter().map(|b| b.text.as_str()).collect();
            out.push(OpenAIMessage::System {
                role: "system".to_string(),
                content: text,
            });
        }
        None => {}
    }

    // Convert each message
    for msg in messages {
        convert_to_openai_messages(msg, &mut out);
    }

    out
}

/// Convert a single internal Message into one or more OpenAI messages
fn convert_to_openai_messages(msg: &Message, out: &mut Vec<OpenAIMessage>) {
    match msg.role.as_str() {
        "user" => match &msg.content {
            MessageContent::Text(s) => {
                out.push(OpenAIMessage::User {
                    role: "user".to_string(),
                    content: s.clone(),
                });
            }
            MessageContent::Blocks(blocks) => {
                // Separate tool results from text/image content
                let mut text_parts: Vec<String> = Vec::new();
                let mut tool_results: Vec<(&str, &str)> = Vec::new();

                for block in blocks {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            text_parts.push(text.clone());
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            let text = content.as_deref().unwrap_or("");
                            tool_results.push((tool_use_id.as_str(), text));
                        }
                        ContentBlock::Image { .. } | ContentBlock::Document { .. } => {
                            // OpenAI doesn't support image/document via this path in the same way;
                            // skip for now with a warning
                            tracing::warn!(
                                "OpenAI provider: image/document blocks not supported in message conversion"
                            );
                        }
                        _ => {}
                    }
                }

                // Emit text content as user message
                if !text_parts.is_empty() {
                    out.push(OpenAIMessage::User {
                        role: "user".to_string(),
                        content: text_parts.join("\n"),
                    });
                }

                // Emit each tool result as a separate tool message
                for (tool_call_id, content) in tool_results {
                    out.push(OpenAIMessage::Tool {
                        role: "tool".to_string(),
                        tool_call_id: tool_call_id.to_string(),
                        content: content.to_string(),
                    });
                }
            }
        },
        "assistant" => match &msg.content {
            MessageContent::Text(s) => {
                // Check if there are tool calls mixed in — shouldn't happen for text-only,
                // but handle gracefully
                out.push(OpenAIMessage::Assistant {
                    role: "assistant".to_string(),
                    content: Some(s.clone()),
                    tool_calls: None,
                });
            }
            MessageContent::Blocks(blocks) => {
                let mut text_content: Option<String> = None;
                let mut tool_calls: Vec<OpenAIToolCall> = Vec::new();

                for block in blocks {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            text_content = Some(text.clone());
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            tool_calls.push(OpenAIToolCall {
                                id: id.clone(),
                                call_type: "function".to_string(),
                                function: OpenAIFunctionCall {
                                    name: name.clone(),
                                    arguments: input.to_string(),
                                },
                            });
                        }
                        _ => {}
                    }
                }

                out.push(OpenAIMessage::Assistant {
                    role: "assistant".to_string(),
                    content: text_content,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                });
            }
        },
        _ => {
            // Unknown role, add as user message
            tracing::warn!("Unknown message role '{}', treating as user", msg.role);
            if let Some(text) = msg.text() {
                out.push(OpenAIMessage::User {
                    role: "user".to_string(),
                    content: text.to_string(),
                });
            }
        }
    }
}

/// Convert internal ToolDefinition to OpenAI tools
fn convert_tools_to_openai(tools: &[ToolDefinition]) -> Vec<OpenAITool> {
    tools
        .iter()
        .map(|tool| {
            let ToolDefinition::Custom(custom) = tool;
            let description = custom.description.clone().unwrap_or_default();
            let parameters = serde_json::to_value(&custom.input_schema).unwrap_or(json!({
                "type": "object"
            }));

            OpenAITool {
                tool_type: "function".to_string(),
                function: OpenAIFunction {
                    name: custom.name.clone(),
                    description: Some(description),
                    parameters,
                },
            }
        })
        .collect()
}

/// Convert internal ToolChoice to OpenAI tool_choice value
fn convert_tool_choice_to_openai(choice: &Option<ToolChoice>) -> Option<Value> {
    match choice {
        None | Some(ToolChoice::Auto { .. }) => Some(json!("auto")),
        Some(ToolChoice::Any { .. }) => Some(json!("required")),
        Some(ToolChoice::Tool { name, .. }) => Some(json!({
            "type": "function",
            "function": { "name": name }
        })),
        Some(ToolChoice::None) => Some(json!("none")),
    }
}

/// Derive the models list URL from the chat completions base URL.
///
/// For standard OpenAI: `https://api.openai.com/v1/chat/completions` → `https://api.openai.com/v1/models`
/// For a custom proxy: `https://proxy.example.com/v1/chat/completions` → `https://proxy.example.com/v1/models`
fn derive_models_url(chat_url: &str) -> String {
    // If the URL ends with /chat/completions, replace it with /models
    if let Some(base) = chat_url.strip_suffix("/chat/completions") {
        return format!("{}/models", base);
    }
    // If the URL ends with /v1/chat/completions (alternate), same logic
    if let Some(base) = chat_url.strip_suffix("chat/completions") {
        return format!("{}models", base);
    }
    // Otherwise, try appending /models to the base (stripping trailing slash)
    let trimmed = chat_url.trim_end_matches('/');
    format!("{}/models", trimmed)
}
