//! OpenAI Responses API wire types and streaming translation.

use anyhow::{Context, Result};
use futures::stream::Stream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::pin::Pin;
use tokio::io::AsyncBufReadExt;
use tokio_util::io::StreamReader;

use crate::auth::AuthConfig;
use crate::openai::{OpenAIProvider, OpenAIReasoningEffort};
use crate::types::{
    ContentBlock, ContentBlockDeltaEvent, ContentBlockStart, ContentBlockStartEvent,
    ContentBlockStopEvent, ContentDelta, DeltaUsage, Message, MessageContent, MessageDeltaData,
    MessageDeltaEvent, MessageStartData, MessageStartEvent, StopReason, StreamEvent, SystemPrompt,
    ThinkingConfig, ToolChoice, ToolDefinition, Usage,
};

pub(crate) const DEFAULT_API_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const REASONING_SIGNATURE_PREFIX: &str = "openai-responses:";
pub(crate) const ORIGINATOR: &str = "omega";

// ============================================================================
// Request wire types
// ============================================================================

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<String>,
    store: bool,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoning {
    effort: String,
    summary: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum InputItem {
    #[serde(rename = "message")]
    Message { role: String, content: InputContent },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        summary: Vec<ReasoningSummary>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum InputContent {
    Text(String),
    Parts(Vec<InputContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum InputContentPart {
    #[serde(rename = "input_text")]
    Text { text: String },
    #[serde(rename = "input_image")]
    Image { image_url: String },
    #[serde(rename = "input_file")]
    File { filename: String, file_data: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReasoningSummary {
    #[serde(rename = "type")]
    summary_type: String,
    text: String,
}

#[derive(Debug, Serialize)]
struct ResponsesTool {
    #[serde(rename = "type")]
    tool_type: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
    strict: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReasoningSignature {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted_content: Option<String>,
}

// ============================================================================
// Response and streaming wire types
// ============================================================================

#[derive(Debug, Default, Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Debug, Default, Deserialize)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

#[derive(Debug, Default, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    output: Vec<OutputItem>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<ResponsesError>,
}

#[derive(Debug, Default, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Default, Deserialize)]
struct ResponsesError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OutputItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        content: Vec<OutputContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        id: String,
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        id: String,
        #[serde(default)]
        summary: Vec<ReasoningSummary>,
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OutputContentPart {
    #[serde(rename = "output_text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "refusal")]
    Refusal {
        #[serde(default)]
        refusal: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated { response: ResponsesResponse },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        output_index: usize,
        item: OutputItem,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { output_index: usize, delta: String },
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta { output_index: usize, delta: String },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { output_index: usize, delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta { output_index: usize, delta: String },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta { output_index: usize, delta: String },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        output_index: usize,
        item: OutputItem,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted { response: ResponsesResponse },
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete { response: ResponsesResponse },
    #[serde(rename = "response.failed")]
    ResponseFailed { response: ResponsesResponse },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        code: String,
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Unknown,
}

// ============================================================================
// Request construction
// ============================================================================

fn build_request(
    provider: &OpenAIProvider,
    messages: Vec<Message>,
    system: Option<SystemPrompt>,
    tools: Vec<ToolDefinition>,
    tool_choice: Option<ToolChoice>,
    thinking: Option<ThinkingConfig>,
    session_id: Option<&str>,
) -> ResponsesRequest {
    let responses_tools = tools.into_iter().map(tool_to_responses).collect::<Vec<_>>();
    let reasoning = provider
        .reasoning_effort
        .map(explicit_reasoning_config)
        .or_else(|| thinking.map(reasoning_config));

    ResponsesRequest {
        model: provider.model.clone(),
        input: messages_to_input_items(messages),
        instructions: system.map(system_prompt_to_string),
        tools: (!responses_tools.is_empty()).then_some(responses_tools),
        tool_choice: tool_choice.map(tool_choice_to_responses),
        max_output_tokens: provider.max_tokens,
        reasoning,
        // This lets stateless callers carry opaque reasoning items across tool
        // turns. Non-reasoning models simply do not return such an item.
        include: Some(vec!["reasoning.encrypted_content".to_string()]),
        prompt_cache_key: session_id.map(str::to_string),
        prompt_cache_retention: session_id.map(|_| "24h".to_string()),
        store: false,
        stream: true,
    }
}

fn reasoning_config(config: ThinkingConfig) -> ResponsesReasoning {
    let effort = match config.budget_tokens {
        0..=2048 => "low",
        2049..=8192 => "medium",
        _ => "high",
    };
    ResponsesReasoning {
        effort: effort.to_string(),
        summary: "auto".to_string(),
    }
}

fn explicit_reasoning_config(effort: OpenAIReasoningEffort) -> ResponsesReasoning {
    ResponsesReasoning {
        effort: effort.as_str().to_string(),
        summary: "auto".to_string(),
    }
}

fn system_prompt_to_string(system: SystemPrompt) -> String {
    match system {
        SystemPrompt::Text(text) => text,
        SystemPrompt::Blocks(blocks) => blocks
            .into_iter()
            .map(|block| block.text)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn messages_to_input_items(messages: Vec<Message>) -> Vec<InputItem> {
    let mut items = Vec::new();

    for message in messages {
        let role = normalize_role(&message.role).to_string();
        let assistant = role == "assistant";
        match message.content {
            MessageContent::Text(text) => items.push(InputItem::Message {
                role,
                content: InputContent::Text(text),
            }),
            MessageContent::Blocks(blocks) => {
                let mut parts = Vec::new();
                let mut assistant_text = Vec::new();

                for block in blocks {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if assistant {
                                assistant_text.push(text);
                            } else {
                                parts.push(InputContentPart::Text { text });
                            }
                        }
                        ContentBlock::Image { source, .. } if !assistant => {
                            parts.push(InputContentPart::Image {
                                image_url: format!(
                                    "data:{};base64,{}",
                                    source.media_type, source.data
                                ),
                            });
                        }
                        ContentBlock::Document { source, .. } if !assistant => {
                            parts.push(InputContentPart::File {
                                filename: "document.pdf".to_string(),
                                file_data: source.data,
                            });
                        }
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } if assistant => {
                            flush_message_content(
                                &mut items,
                                &role,
                                &mut parts,
                                &mut assistant_text,
                            );
                            if let Some(reasoning) = decode_reasoning_signature(&signature) {
                                items.push(InputItem::Reasoning {
                                    id: reasoning.id,
                                    summary: (!thinking.is_empty())
                                        .then_some(vec![ReasoningSummary {
                                            summary_type: "summary_text".to_string(),
                                            text: thinking,
                                        }])
                                        .unwrap_or_default(),
                                    encrypted_content: reasoning.encrypted_content,
                                });
                            }
                        }
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            signature,
                        } => {
                            flush_message_content(
                                &mut items,
                                &role,
                                &mut parts,
                                &mut assistant_text,
                            );
                            items.push(InputItem::FunctionCall {
                                id: signature.filter(|value| value.starts_with("fc_")),
                                call_id: id,
                                name,
                                arguments: serde_json::to_string(&input)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            });
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            flush_message_content(
                                &mut items,
                                &role,
                                &mut parts,
                                &mut assistant_text,
                            );
                            items.push(InputItem::FunctionCallOutput {
                                call_id: tool_use_id,
                                output: content.unwrap_or_default(),
                            });
                        }
                        ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::Document { .. } => {}
                    }
                }

                flush_message_content(&mut items, &role, &mut parts, &mut assistant_text);
            }
        }
    }

    items
}

fn flush_message_content(
    items: &mut Vec<InputItem>,
    role: &str,
    parts: &mut Vec<InputContentPart>,
    assistant_text: &mut Vec<String>,
) {
    if !assistant_text.is_empty() {
        items.push(InputItem::Message {
            role: role.to_string(),
            content: InputContent::Text(std::mem::take(assistant_text).join("")),
        });
    }
    if !parts.is_empty() {
        items.push(InputItem::Message {
            role: role.to_string(),
            content: InputContent::Parts(std::mem::take(parts)),
        });
    }
}

fn normalize_role(role: &str) -> &str {
    match role {
        "assistant" | "system" | "developer" => role,
        _ => "user",
    }
}

fn tool_to_responses(tool: ToolDefinition) -> ResponsesTool {
    let ToolDefinition::Custom(custom) = tool;
    ResponsesTool {
        tool_type: "function".to_string(),
        name: custom.name,
        description: custom.description,
        parameters: serde_json::to_value(custom.input_schema)
            .unwrap_or_else(|_| json!({ "type": "object" })),
        strict: false,
    }
}

fn tool_choice_to_responses(choice: ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto { .. } => json!("auto"),
        ToolChoice::Any { .. } => json!("required"),
        ToolChoice::None => json!("none"),
        ToolChoice::Tool { name, .. } => json!({ "type": "function", "name": name }),
    }
}

fn encode_reasoning_signature(id: String, encrypted_content: Option<String>) -> String {
    let signature = ReasoningSignature {
        id,
        encrypted_content,
    };
    format!(
        "{REASONING_SIGNATURE_PREFIX}{}",
        serde_json::to_string(&signature).unwrap_or_default()
    )
}

fn decode_reasoning_signature(signature: &str) -> Option<ReasoningSignature> {
    let encoded = signature.strip_prefix(REASONING_SIGNATURE_PREFIX)?;
    serde_json::from_str(encoded).ok()
}

// ============================================================================
// Streaming translation
// ============================================================================

#[derive(Default)]
struct TranslationState {
    text_deltas: HashSet<usize>,
    argument_deltas: HashSet<usize>,
    reasoning_deltas: HashSet<usize>,
    saw_refusal: bool,
}

fn translate_event(
    event: ResponsesStreamEvent,
    fallback_model: &str,
    state: &mut TranslationState,
) -> Result<Vec<StreamEvent>> {
    match event {
        ResponsesStreamEvent::ResponseCreated { response } => {
            let usage = response.usage.unwrap_or_default();
            Ok(vec![StreamEvent::MessageStart(MessageStartEvent {
                message: MessageStartData {
                    id: response.id,
                    message_type: "message".to_string(),
                    role: "assistant".to_string(),
                    content: vec![],
                    model: if response.model.is_empty() {
                        fallback_model.to_string()
                    } else {
                        response.model
                    },
                    stop_reason: None,
                    stop_sequence: None,
                    usage: usage_to_message_usage(&usage),
                },
            })])
        }
        ResponsesStreamEvent::OutputItemAdded { output_index, item } => {
            let content_block = match item {
                OutputItem::Message { .. } => ContentBlockStart::Text {
                    text: String::new(),
                },
                OutputItem::FunctionCall {
                    id, call_id, name, ..
                } => ContentBlockStart::ToolUse {
                    id: call_id,
                    name,
                    input: Value::Null,
                    signature: (!id.is_empty()).then_some(id),
                },
                OutputItem::Reasoning { .. } => ContentBlockStart::Thinking {
                    thinking: String::new(),
                },
                OutputItem::Unknown => return Ok(vec![]),
            };
            Ok(vec![StreamEvent::ContentBlockStart(
                ContentBlockStartEvent {
                    index: output_index,
                    content_block,
                },
            )])
        }
        ResponsesStreamEvent::OutputTextDelta {
            output_index,
            delta,
        } => {
            state.text_deltas.insert(output_index);
            Ok(vec![StreamEvent::ContentBlockDelta(
                ContentBlockDeltaEvent {
                    index: output_index,
                    delta: ContentDelta::TextDelta { text: delta },
                },
            )])
        }
        ResponsesStreamEvent::RefusalDelta {
            output_index,
            delta,
        } => {
            state.text_deltas.insert(output_index);
            state.saw_refusal = true;
            Ok(vec![StreamEvent::ContentBlockDelta(
                ContentBlockDeltaEvent {
                    index: output_index,
                    delta: ContentDelta::TextDelta { text: delta },
                },
            )])
        }
        ResponsesStreamEvent::FunctionCallArgumentsDelta {
            output_index,
            delta,
        } => {
            state.argument_deltas.insert(output_index);
            Ok(vec![StreamEvent::ContentBlockDelta(
                ContentBlockDeltaEvent {
                    index: output_index,
                    delta: ContentDelta::InputJsonDelta {
                        partial_json: delta,
                    },
                },
            )])
        }
        ResponsesStreamEvent::ReasoningSummaryTextDelta {
            output_index,
            delta,
        }
        | ResponsesStreamEvent::ReasoningTextDelta {
            output_index,
            delta,
        } => {
            state.reasoning_deltas.insert(output_index);
            Ok(vec![StreamEvent::ContentBlockDelta(
                ContentBlockDeltaEvent {
                    index: output_index,
                    delta: ContentDelta::ThinkingDelta { thinking: delta },
                },
            )])
        }
        ResponsesStreamEvent::OutputItemDone { output_index, item } => {
            let mut events = Vec::new();
            match item {
                OutputItem::Message { content, .. }
                    if !state.text_deltas.contains(&output_index) =>
                {
                    for part in content {
                        let text = match part {
                            OutputContentPart::Text { text } => text,
                            OutputContentPart::Refusal { refusal } => {
                                state.saw_refusal = true;
                                refusal
                            }
                            OutputContentPart::Unknown => continue,
                        };
                        if !text.is_empty() {
                            events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                                index: output_index,
                                delta: ContentDelta::TextDelta { text },
                            }));
                        }
                    }
                }
                OutputItem::FunctionCall { arguments, .. }
                    if !state.argument_deltas.contains(&output_index) && !arguments.is_empty() =>
                {
                    events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                        index: output_index,
                        delta: ContentDelta::InputJsonDelta {
                            partial_json: arguments,
                        },
                    }));
                }
                OutputItem::Reasoning {
                    id,
                    summary,
                    encrypted_content,
                } => {
                    if !state.reasoning_deltas.contains(&output_index) {
                        let text = summary
                            .iter()
                            .map(|part| part.text.as_str())
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.is_empty() {
                            events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                                index: output_index,
                                delta: ContentDelta::ThinkingDelta { thinking: text },
                            }));
                        }
                    }
                    if !id.is_empty() {
                        events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                            index: output_index,
                            delta: ContentDelta::SignatureDelta {
                                signature: encode_reasoning_signature(id, encrypted_content),
                            },
                        }));
                    }
                }
                OutputItem::Unknown => return Ok(vec![]),
                _ => {}
            }
            events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                index: output_index,
            }));
            Ok(events)
        }
        ResponsesStreamEvent::ResponseCompleted { response }
        | ResponsesStreamEvent::ResponseIncomplete { response } => {
            let stop_reason = response_stop_reason(&response, state.saw_refusal);
            let usage = response.usage.unwrap_or_default();
            Ok(vec![
                StreamEvent::MessageDelta(MessageDeltaEvent {
                    delta: MessageDeltaData {
                        stop_reason: Some(stop_reason),
                        stop_sequence: None,
                    },
                    usage: DeltaUsage {
                        output_tokens: usage.output_tokens,
                        input_tokens: Some(usage.input_tokens),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: usage
                            .input_tokens_details
                            .map(|details| details.cached_tokens),
                    },
                }),
                StreamEvent::MessageStop,
            ])
        }
        ResponsesStreamEvent::ResponseFailed { response } => {
            let error = response.error.unwrap_or_default();
            anyhow::bail!(
                "OpenAI Responses API stream failed ({}): {}",
                error.code,
                error.message
            )
        }
        ResponsesStreamEvent::Error { code, message } => {
            anyhow::bail!("OpenAI Responses API stream error ({code}): {message}")
        }
        ResponsesStreamEvent::Unknown => Ok(vec![]),
    }
}

fn usage_to_message_usage(usage: &ResponsesUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: usage
            .input_tokens_details
            .as_ref()
            .map(|details| details.cached_tokens),
        thoughts_token_count: usage
            .output_tokens_details
            .as_ref()
            .map(|details| details.reasoning_tokens)
            .filter(|tokens| *tokens > 0),
    }
}

fn response_stop_reason(response: &ResponsesResponse, saw_refusal: bool) -> StopReason {
    if response
        .output
        .iter()
        .any(|item| matches!(item, OutputItem::FunctionCall { .. }))
    {
        return StopReason::ToolUse;
    }
    if saw_refusal
        || response.output.iter().any(|item| match item {
            OutputItem::Message { content, .. } => content
                .iter()
                .any(|part| matches!(part, OutputContentPart::Refusal { .. })),
            _ => false,
        })
    {
        return StopReason::Refusal;
    }
    if response.status == "incomplete" {
        return match response
            .incomplete_details
            .as_ref()
            .map(|details| details.reason.as_str())
        {
            Some("content_filter") => StopReason::Refusal,
            _ => StopReason::MaxTokens,
        };
    }
    StopReason::EndTurn
}

// ============================================================================
// HTTP entry point
// ============================================================================

fn add_responses_headers(
    mut request: reqwest::RequestBuilder,
    auth: &AuthConfig,
    session_id: Option<&str>,
) -> reqwest::RequestBuilder {
    request = request
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("originator", ORIGINATOR)
        .header("Authorization", format!("Bearer {}", auth.api_key));
    if let Some(account_id) = auth.account_id.as_deref() {
        request = request.header("ChatGPT-Account-ID", account_id);
    }
    if let Some(session_id) = session_id {
        request = request.header("session-id", session_id);
    }
    request
}

async fn send_responses_request(
    provider: &OpenAIProvider,
    api_url: &str,
    auth: &AuthConfig,
    session_id: Option<&str>,
    request: &ResponsesRequest,
) -> Result<reqwest::Response> {
    add_responses_headers(provider.client.post(api_url), auth, session_id)
        .json(request)
        .send()
        .await
        .context("Failed to send streaming request to OpenAI Responses API")
}

pub(crate) async fn stream_with_tools_and_system(
    provider: &OpenAIProvider,
    messages: Vec<Message>,
    system: Option<SystemPrompt>,
    tools: Vec<ToolDefinition>,
    tool_choice: Option<ToolChoice>,
    thinking: Option<ThinkingConfig>,
    session_id: Option<&str>,
) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
    let mut auth = provider
        .auth
        .get_auth()
        .await
        .context("Failed to get authentication credentials")?;
    let request = build_request(
        provider,
        messages,
        system,
        tools,
        tool_choice,
        thinking,
        session_id,
    );

    let api_url = auth.base_url.as_deref().unwrap_or(DEFAULT_API_URL);
    let mut response =
        send_responses_request(provider, api_url, &auth, session_id, &request).await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED && provider.auth.supports_refresh() {
        drop(response);
        auth = provider
            .auth
            .refresh_auth()
            .await
            .context("Failed to refresh rejected Codex OAuth credentials")?;
        let retry_url = auth.base_url.as_deref().unwrap_or(DEFAULT_API_URL);
        response = send_responses_request(provider, retry_url, &auth, session_id, &request)
            .await
            .context("Failed to retry Responses request after refreshing Codex OAuth")?;
    }

    let status = response.status();
    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read error body".to_string());
        anyhow::bail!("OpenAI Responses API error ({}): {}", status, error_text);
    }

    let fallback_model = provider.model.clone();
    let byte_stream = response.bytes_stream();
    let stream_reader = StreamReader::new(
        byte_stream.map(|result| result.map_err(|error| std::io::Error::other(error.to_string()))),
    );
    let buf_reader = tokio::io::BufReader::new(stream_reader);

    let stream = async_stream::try_stream! {
        let mut lines = buf_reader.lines();
        let mut state = TranslationState::default();

        while let Some(line) = lines.next_line().await? {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();
            if data == "[DONE]" {
                break;
            }

            match serde_json::from_str::<ResponsesStreamEvent>(data) {
                Ok(event) => {
                    for event in translate_event(event, &fallback_model, &mut state)? {
                        yield event;
                    }
                }
                Err(error) => tracing::warn!(
                    "Failed to parse OpenAI Responses SSE event: {} — data: {}",
                    error,
                    data
                ),
            }
        }
    };

    Ok(Box::pin(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::OpenAIApiType;
    use crate::types::{CustomTool, ToolInputSchema};

    fn provider() -> OpenAIProvider {
        OpenAIProvider::new("test-key")
            .with_model("gpt-test")
            .with_api_type(OpenAIApiType::Responses)
            .with_max_tokens(Some(4096))
    }

    #[test]
    fn request_uses_responses_fields_and_session_cache_key() {
        let request = build_request(
            &provider(),
            vec![Message::user("hello")],
            Some(SystemPrompt::Text("be helpful".to_string())),
            vec![],
            None,
            Some(ThinkingConfig::enabled(4096)),
            Some("session-123"),
        );
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["model"], "gpt-test");
        assert_eq!(json["input"][0]["type"], "message");
        assert_eq!(json["instructions"], "be helpful");
        assert_eq!(json["max_output_tokens"], 4096);
        assert_eq!(json["reasoning"]["effort"], "medium");
        assert_eq!(json["prompt_cache_key"], "session-123");
        assert_eq!(json["prompt_cache_retention"], "24h");
        assert_eq!(json["store"], false);
        assert_eq!(json["stream"], true);
    }

    #[test]
    fn explicit_reasoning_effort_overrides_thinking_budget() {
        let provider = provider().with_reasoning_effort(OpenAIReasoningEffort::Medium);
        let request = build_request(
            &provider,
            vec![Message::user("hello")],
            None,
            vec![],
            None,
            Some(ThinkingConfig::enabled(16_000)),
            None,
        );
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["reasoning"]["effort"], "medium");
    }

    #[test]
    fn request_headers_include_codex_oauth_credentials() {
        let auth = AuthConfig::new("oauth-token").with_account_id("account-123");
        let request = add_responses_headers(
            reqwest::Client::new().post(DEFAULT_API_URL),
            &auth,
            Some("session-123"),
        )
        .build()
        .unwrap();

        assert_eq!(
            request
                .headers()
                .get("Authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer oauth-token")
        );
        assert_eq!(
            request
                .headers()
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("account-123")
        );
        assert_eq!(
            request
                .headers()
                .get("Accept")
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(
            request
                .headers()
                .get("originator")
                .and_then(|value| value.to_str().ok()),
            Some("omega")
        );
        assert_eq!(
            request
                .headers()
                .get("session-id")
                .and_then(|value| value.to_str().ok()),
            Some("session-123")
        );
    }

    #[test]
    fn request_serializes_responses_tool_shape() {
        let tool = ToolDefinition::Custom(CustomTool {
            name: "read_file".to_string(),
            description: Some("Read a file".to_string()),
            input_schema: ToolInputSchema::new()
                .with_properties(json!({ "path": { "type": "string" } }))
                .with_required(vec!["path".to_string()]),
            tool_type: None,
            cache_control: None,
        });
        let request = build_request(
            &provider(),
            vec![Message::user("read it")],
            None,
            vec![tool],
            Some(ToolChoice::tool("read_file")),
            None,
            None,
        );
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["name"], "read_file");
        assert_eq!(json["tools"][0]["parameters"]["type"], "object");
        assert_eq!(json["tool_choice"]["name"], "read_file");
    }

    #[test]
    fn tool_call_item_id_round_trips_through_signature() {
        let message = Message::assistant_with_blocks(vec![ContentBlock::ToolUse {
            id: "call_123".to_string(),
            name: "read_file".to_string(),
            input: json!({ "path": "README.md" }),
            signature: Some("fc_123".to_string()),
        }]);
        let json = serde_json::to_value(messages_to_input_items(vec![message])).unwrap();

        assert_eq!(json[0]["type"], "function_call");
        assert_eq!(json[0]["id"], "fc_123");
        assert_eq!(json[0]["call_id"], "call_123");
    }

    #[test]
    fn user_images_and_documents_use_responses_content_parts() {
        let message = Message::user_with_blocks(vec![
            ContentBlock::image("image-bytes".to_string(), "image/png".to_string()),
            ContentBlock::document("document-bytes".to_string(), "application/pdf".to_string()),
        ]);
        let json = serde_json::to_value(messages_to_input_items(vec![message])).unwrap();

        assert_eq!(
            json[0]["content"][0]["image_url"],
            "data:image/png;base64,image-bytes"
        );
        assert_eq!(json[0]["content"][1]["type"], "input_file");
        assert_eq!(json[0]["content"][1]["file_data"], "document-bytes");
    }

    #[test]
    fn streamed_function_call_translates_to_tool_events_and_full_usage() {
        let mut state = TranslationState::default();
        let added: ResponsesStreamEvent = serde_json::from_value(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_123",
                "call_id": "call_123",
                "name": "read_file",
                "arguments": ""
            }
        }))
        .unwrap();
        let events = translate_event(added, "gpt-test", &mut state).unwrap();
        match &events[0] {
            StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                content_block:
                    ContentBlockStart::ToolUse {
                        id,
                        name,
                        signature,
                        ..
                    },
                ..
            }) => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "read_file");
                assert_eq!(signature.as_deref(), Some("fc_123"));
            }
            _ => panic!("expected tool-use start"),
        }

        let completed: ResponsesStreamEvent = serde_json::from_value(json!({
            "type": "response.completed",
            "response": {
                "id": "resp_123",
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "id": "fc_123",
                    "call_id": "call_123",
                    "name": "read_file",
                    "arguments": "{\"path\":\"README.md\"}"
                }],
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "input_tokens_details": { "cached_tokens": 80 }
                }
            }
        }))
        .unwrap();
        let events = translate_event(completed, "gpt-test", &mut state).unwrap();
        match &events[0] {
            StreamEvent::MessageDelta(event) => {
                assert_eq!(event.delta.stop_reason, Some(StopReason::ToolUse));
                assert_eq!(event.usage.input_tokens, Some(100));
                assert_eq!(event.usage.output_tokens, 20);
                assert_eq!(event.usage.cache_read_input_tokens, Some(80));
            }
            _ => panic!("expected message delta"),
        }
        assert!(matches!(events[1], StreamEvent::MessageStop));
    }

    #[test]
    fn reasoning_signature_round_trips() {
        let encoded =
            encode_reasoning_signature("rs_123".to_string(), Some("opaque-data".to_string()));
        let decoded = decode_reasoning_signature(&encoded).unwrap();
        assert_eq!(decoded.id, "rs_123");
        assert_eq!(decoded.encrypted_content.as_deref(), Some("opaque-data"));
    }
}
