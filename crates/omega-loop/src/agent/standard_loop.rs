//! Standard Agent Loop
//!
//! The main agent implementation that handles:
//! - Input → LLM → Tools → Output cycle
//! - Context injection before LLM calls
//! - Session persistence
//! - Debug logging (when enabled)
//! - Streaming responses (when enabled)
//! - Automatic conversation naming (after first turn)

use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use serde_json::Value;

use crate::helpers::{process_attachments, Debugger};
use crate::runtime::AgentInternals;
use omega_core::core::{FrameworkResult, InputMessage};
use omega_core::core::{ToolResult, ToolResultData};
use omega_llm::{
    CacheControl, ContentBlock, ContentBlockStart, ContentDelta, LlmProvider, Message, StopReason,
    StreamEvent, SystemBlock, SystemPrompt,
};

use super::config::AgentConfig;
use super::executor::ToolExecutor;

/// Standard agent that handles the full agent loop
///
/// # Example
///
/// ```ignore
/// let config = AgentConfig::new("You are helpful")
///     .with_tools(tools);
///
/// let agent = StandardAgent::new(config, llm);
///
/// let handle = runtime.spawn(session, |internals| {
///     agent.run(internals)
/// }).await;
/// ```
pub struct StandardAgent {
    config: AgentConfig,
    llm: Arc<dyn LlmProvider>,
}

impl StandardAgent {
    /// Create a new standard agent
    pub fn new(config: AgentConfig, llm: Arc<dyn LlmProvider>) -> Self {
        Self { config, llm }
    }

    /// Run the agent loop
    ///
    /// This is the main entry point - pass this to `runtime.spawn()`.
    pub async fn run(self, mut internals: AgentInternals) -> FrameworkResult<()> {
        tracing::info!("[StandardAgent] Started, waiting for input...");

        // Write initial model/provider info into session metadata
        {
            let mut session = internals.session.write().await;
            session.set_model(self.llm.model());
            session.set_provider(self.llm.provider_name());
        }

        // Initialize debugger if enabled
        if self.config.debug_enabled {
            let session = internals.session.read().await;
            let session_dir = session.storage().session_dir(session.session_id());
            drop(session);

            match Debugger::new(&session_dir) {
                Ok(debugger) => {
                    tracing::info!(
                        "[StandardAgent] Debug logging enabled at {:?}",
                        debugger.dir()
                    );
                    internals.context.insert_resource(debugger);
                }
                Err(e) => {
                    tracing::warn!("[StandardAgent] Failed to initialize debugger: {}", e);
                }
            }
        }

        loop {
            // Signal we're ready for input
            internals.set_idle().await;

            // Wait for next message
            match internals.receive().await {
                Some(InputMessage::UserInput(text)) => {
                    tracing::info!("[StandardAgent] Received: {}", text);
                    internals.set_processing().await;

                    // Process the user message
                    let retry_config = &self.config.turn_retry;
                    let mut attempt = 0u32;
                    let mut first_attempt = true;
                    loop {
                        match self
                            .process_turn(&mut internals, &text, first_attempt)
                            .await
                        {
                            Ok(()) => break,
                            Err(e) => {
                                first_attempt = false;
                                attempt += 1;
                                let err_msg = e.to_string();
                                let is_transient = err_msg.contains("error decoding response body")
                                    || err_msg.contains("connection")
                                    || err_msg.contains("timeout")
                                    || err_msg.contains("broken pipe")
                                    || err_msg.contains("reset by peer")
                                    || err_msg.contains("stream")
                                    || err_msg.contains("hyper")
                                    || err_msg.contains("io error");

                                if retry_config.enabled
                                    && is_transient
                                    && attempt < retry_config.max_retries
                                {
                                    tracing::warn!(
                                            "[StandardAgent] Transient error on attempt {}/{}: {}. Retrying in {}s...",
                                            attempt, retry_config.max_retries, e, retry_config.retry_delay_secs
                                        );
                                    internals.send_status(format!(
                                        "Connection issue, retrying... (attempt {}/{})",
                                        attempt, retry_config.max_retries
                                    ));
                                    tokio::time::sleep(std::time::Duration::from_secs(
                                        retry_config.retry_delay_secs,
                                    ))
                                    .await;
                                    continue;
                                }

                                tracing::error!("[StandardAgent] Error processing turn: {}", e);
                                internals.send_error(format!("Error: {}", e));
                                break;
                            }
                        }
                    }

                    // Conversation naming removed — it caused a 30-second blocking
                    // LLM call before send_done(), freezing the TUI.
                    // Signal turn complete
                    internals.send_done();

                    // Persist session if configured
                    if self.config.auto_save_session {
                        if let Err(e) = internals.session.write().await.save() {
                            tracing::error!("[StandardAgent] Failed to save session: {}", e);
                        }
                    }
                }

                Some(InputMessage::Interrupt) => {
                    // Steering interrupt: the user pressed Esc while the agent
                    // was idle (between turns). This must NOT kill the agent —
                    // it just acknowledges and keeps waiting for the next
                    // message so the user can immediately steer with new input.
                    tracing::info!("[StandardAgent] Interrupt received while idle");
                    internals.send_status("Interrupted — type a message to steer");
                }

                Some(InputMessage::Shutdown) | None => {
                    tracing::info!("[StandardAgent] Shutting down");
                    internals.set_done().await;
                    break;
                }

                _ => {
                    // Ignore other message types
                }
            }

            internals.next_turn();
        }

        Ok(())
    }

    /// Process a single user turn (may involve multiple LLM calls for tool use)
    ///
    /// `add_user_message`: true on the first attempt, false on retries to avoid
    /// duplicating the user message in session history.
    async fn process_turn(
        &self,
        internals: &mut AgentInternals,
        user_input: &str,
        add_user_message: bool,
    ) -> Result<()> {
        // Only add the user message on the first attempt (not on retries)
        if add_user_message {
            // Check if input contains attachment tags and process them
            let user_message = if user_input.contains("<vibe-work-attachment>") {
                tracing::info!("[StandardAgent] Processing attachments in user input");

                // Get base directory from current working directory
                let base_dir = std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .to_string_lossy()
                    .to_string();

                // Process attachments
                let attachment_blocks = process_attachments(user_input, &base_dir);

                // Build message blocks: original text first, then attachments
                let mut blocks = vec![ContentBlock::Text {
                    text: user_input.to_string(),
                    cache_control: None,
                }];
                blocks.extend(attachment_blocks);

                Message::user_with_blocks(blocks)
            } else {
                // No attachments, use simple text message
                Message::user(user_input)
            };

            // Add user message to history
            internals.session.write().await.add_message(user_message)?;
        }

        // Get tool definitions
        let tool_definitions = self.config.tool_definitions();

        let mut iterations = 0;

        // LLM loop - continues until no more tool calls
        loop {
            iterations += 1;
            if iterations > self.config.max_tool_iterations {
                tracing::warn!(
                    "[StandardAgent] Max tool iterations ({}) reached",
                    self.config.max_tool_iterations
                );
                internals.send_status("Max tool iterations reached");
                break;
            }

            // Get messages and system prompt from session
            let (messages, system_prompt_text) = {
                let session = internals.session.read().await;
                (
                    session.history().to_vec(),
                    session.system_prompt().to_string(),
                )
            };

            // IMPORTANT: Apply cache control BEFORE injections
            // This ensures we cache the stable message content (without dynamic injections)
            // The injections will be added AFTER the cache breakpoint, so they're sent but not cached
            // This allows the cache to match across turns even though injections are dynamic
            let (tools_with_cache, system_with_cache, mut messages_with_cache) =
                self.apply_cache_control(&system_prompt_text, tool_definitions.to_vec(), messages);

            // Apply context injections AFTER cache control
            messages_with_cache = self.config.injections.apply(internals, messages_with_cache);

            // Update session metadata with current model/provider (may change via SwappableLlmProvider)
            {
                let mut session = internals.session.write().await;
                session.set_model(self.llm.model());
                session.set_provider(self.llm.provider_name());
            }

            tracing::info!(
                "[StandardAgent] Calling LLM with {} messages (iteration {})",
                messages_with_cache.len(),
                iterations
            );

            // Log API request if debugger is enabled (with cache_control included)
            if let Some(debugger) = internals.context.get_resource::<Debugger>() {
                let tool_defs: Vec<serde_json::Value> = tools_with_cache
                    .iter()
                    .map(|t| serde_json::to_value(t).unwrap_or_default())
                    .collect();

                // Convert SystemPrompt to string for logging (or serialize as-is)
                let system_str = match &system_with_cache {
                    Some(SystemPrompt::Text(s)) => Some(s.as_str()),
                    Some(SystemPrompt::Blocks(_)) => {
                        // For blocks, we'll serialize them so cache_control is visible
                        None // Will serialize full structure below
                    }
                    None => None,
                };

                // If we have system blocks, we need to log them differently
                if let Some(SystemPrompt::Blocks(_)) = &system_with_cache {
                    // Log the full request with SystemPrompt blocks
                    if let Err(e) = debugger.log_api_request_full(
                        &messages_with_cache,
                        system_with_cache.clone(),
                        Some(&tool_defs),
                    ) {
                        tracing::warn!("[StandardAgent] Failed to log API request: {}", e);
                    }
                } else {
                    // Legacy path for simple string system prompt
                    if let Err(e) =
                        debugger.log_api_request(&messages_with_cache, system_str, Some(&tool_defs))
                    {
                        tracing::warn!("[StandardAgent] Failed to log API request: {}", e);
                    }
                }
            }

            // Call LLM with streaming (always enabled)
            let (content_blocks, stop_reason) = self
                .call_llm_streaming_with_cache(
                    internals,
                    messages_with_cache,
                    tools_with_cache,
                    system_with_cache,
                )
                .await?;

            tracing::info!(
                "[StandardAgent] LLM response: stop_reason={:?}",
                stop_reason
            );

            // Process tool use blocks and execute tools
            let mut tool_results: Vec<(String, ToolResult)> = Vec::new();

            // Track recent tool calls for loop detection
            let mut tool_call_set = std::collections::HashSet::new();

            for (index, block) in content_blocks.iter().enumerate() {
                if let ContentBlock::ToolUse {
                    id, name, input, ..
                } = block
                {
                    tracing::info!("[StandardAgent] Tool use: {} ({})", name, id);

                    // Loop detection: Check if this exact tool call was already made in this turn
                    let call_signature = format!("{}:{}", name, input);
                    if !tool_call_set.insert(call_signature) {
                        tracing::warn!(
                            "[StandardAgent] Loop detected: duplicate tool call {} with same args",
                            name
                        );
                        internals.send_error(format!(
                            "Loop detected: tool '{}' called multiple times with identical arguments in same turn",
                            name
                        ));
                        // Stop processing further tools and exit the turn
                        return Ok(());
                    }

                    // Execute tool (if tools configured)
                    let result = if let Some(ref tools) = self.config.tools {
                        ToolExecutor::execute(internals, tools, name, id, input).await
                    } else {
                        ToolResult::error(format!("No tools configured, cannot execute: {}", name))
                    };

                    tool_results.push((id.clone(), result));

                    // Check if user interrupted after tool execution (non-blocking check)
                    // Use tokio::select with immediate timeout to check without blocking
                    let interrupt_check = tokio::time::timeout(
                        std::time::Duration::from_millis(0),
                        internals.receive(),
                    );

                    if let Ok(Some(InputMessage::Interrupt)) = interrupt_check.await {
                        tracing::info!("[StandardAgent] Interrupt detected after tool execution");

                        // For all remaining tools that haven't executed, add "Interrupted" error
                        for remaining_block in content_blocks.iter().skip(index + 1) {
                            if let ContentBlock::ToolUse {
                                id: remaining_id, ..
                            } = remaining_block
                            {
                                tool_results
                                    .push((remaining_id.clone(), ToolResult::error("Interrupted")));
                            }
                        }

                        break;
                    }
                }
            }

            // Add assistant message to history
            internals
                .session
                .write()
                .await
                .add_message(Message::assistant_with_blocks(content_blocks.clone()))?;

            // Check if any tool was interrupted
            let has_interrupt = tool_results.iter().any(|(_, result)| {
                result.is_error && matches!(&result.content, ToolResultData::Text(text) if text == "Interrupted")
            });

            if has_interrupt {
                tracing::info!("[StandardAgent] Tool execution interrupted, ending turn");
                // Add the interrupt results to history
                let tool_result_blocks: Vec<ContentBlock> = tool_results
                    .into_iter()
                    .flat_map(|(id, result)| match result.content {
                        ToolResultData::Text(text) => {
                            vec![ContentBlock::tool_result(&id, &text, result.is_error)]
                        }
                        _ => vec![],
                    })
                    .collect();

                internals
                    .session
                    .write()
                    .await
                    .add_message(Message::user_with_blocks(tool_result_blocks))?;

                // Add system message indicating the interrupt
                internals
                    .session
                    .write()
                    .await
                    .add_message(Message::assistant("<vibe-working-agent-system>User interrupted this message</vibe-working-agent-system>"))?;

                // Break out of the loop
                break;
            }

            // If there were tool calls, add results and continue loop
            if !tool_results.is_empty() {
                // Add tool results as a message (WITHOUT cache_control)
                // Cache control will be applied dynamically in apply_cache_control()
                let tool_result_blocks: Vec<ContentBlock> = tool_results
                    .into_iter()
                    .flat_map(|(id, result)| {
                        match result.content {
                            ToolResultData::Text(text) => {
                                vec![ContentBlock::tool_result(&id, &text, result.is_error)]
                            }
                            ToolResultData::Image { data, media_type } => {
                                // Encode image data to base64
                                use base64::Engine;
                                let base64_data =
                                    base64::engine::general_purpose::STANDARD.encode(&data);

                                vec![
                                    ContentBlock::ToolResult {
                                        tool_use_id: id,
                                        content: None,
                                        is_error: if result.is_error { Some(true) } else { None },
                                        cache_control: None,
                                    },
                                    ContentBlock::image(base64_data, media_type),
                                ]
                            }
                            ToolResultData::Document {
                                data,
                                media_type,
                                description,
                            } => {
                                // Encode document data to base64
                                use base64::Engine;
                                let base64_data =
                                    base64::engine::general_purpose::STANDARD.encode(&data);

                                // For PDFs: two separate blocks as per API spec
                                vec![
                                    ContentBlock::tool_result(&id, &description, result.is_error),
                                    ContentBlock::document(base64_data, media_type),
                                ]
                            }
                        }
                    })
                    .collect();

                internals
                    .session
                    .write()
                    .await
                    .add_message(Message::user_with_blocks(tool_result_blocks))?;

                // Continue to next LLM call
                continue;
            }

            // No tool calls - check if we should stop
            match stop_reason {
                Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                    // Done with this turn
                    break;
                }
                Some(StopReason::ToolUse) => {
                    // Shouldn't happen if tool_results is empty, but continue just in case
                    continue;
                }
                Some(StopReason::MaxTokens) => {
                    internals.send_status("Response truncated (max tokens)");
                    break;
                }
                Some(StopReason::PauseTurn) => {
                    // Model paused, wait for next input
                    break;
                }
                Some(StopReason::Refusal) => {
                    internals.send_status("Model refused to respond");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Apply cache control to tools, system prompt, and messages (if enabled).
    ///
    /// Strategy:
    ///   - Up to 2 system/developer messages from the front
    ///   - Last 2 user/assistant messages from the end
    ///   - Cache control on the last tool definition
    ///   - 1-hour TTL for stable hot cache across long sessions
    fn apply_cache_control(
        &self,
        system_prompt_text: &str,
        mut tool_definitions: Vec<omega_llm::ToolDefinition>,
        mut messages: Vec<Message>,
    ) -> (
        Vec<omega_llm::ToolDefinition>,
        Option<SystemPrompt>,
        Vec<Message>,
    ) {
        if !self.config.enable_prompt_caching {
            // Caching disabled - return system prompt as simple text
            return (
                tool_definitions,
                Some(SystemPrompt::Text(system_prompt_text.to_string())),
                messages,
            );
        }

        // Use ephemeral cache control with 1-hour TTL
        let marker = CacheControl::ephemeral_1h();

        // IMPORTANT: Strip ALL existing cache_control from messages first
        // This ensures we don't accidentally create duplicate cache breakpoints
        for message in &mut messages {
            if let omega_llm::MessageContent::Blocks(blocks) = &mut message.content {
                for block in blocks {
                    match block {
                        ContentBlock::Text { cache_control, .. } => {
                            *cache_control = None;
                        }
                        ContentBlock::ToolResult { cache_control, .. } => {
                            *cache_control = None;
                        }
                        _ => {}
                    }
                }
            }
        }

        // 1. Add cache control to last tool definition (caches all tools)
        if let Some(last_tool) = tool_definitions.last_mut() {
            *last_tool = last_tool.clone().with_cache_control(marker.clone());
        }

        // 2. Stamp up to 2 system messages from the front
        let mut system_stamped = 0;
        for msg in messages.iter_mut() {
            if msg.role == "system" || msg.role == "developer" {
                if stamp_message_end(&marker, msg) {
                    system_stamped += 1;
                    if system_stamped >= 2 {
                        break;
                    }
                }
            } else {
                break;
            }
        }

        // 3. Stamp last 2 user/assistant messages from the end
        let mut final_stamped = 0;
        for msg in messages.iter_mut().rev() {
            if msg.role == "user" || msg.role == "assistant" {
                if stamp_message_end(&marker, msg) {
                    final_stamped += 1;
                    if final_stamped >= 2 {
                        break;
                    }
                }
            }
        }

        // 4. System prompt with cache control (as a single block with marker)
        let system_prompt = Some(SystemPrompt::Blocks(vec![SystemBlock::new(
            system_prompt_text.to_string(),
        )
        .with_cache_control(marker.clone())]));

        (tool_definitions, system_prompt, messages)
    }

    /// Call LLM with streaming - sends deltas in real-time
    async fn call_llm_streaming_with_cache(
        &self,
        internals: &mut AgentInternals,
        messages: Vec<Message>,
        tools: Vec<omega_llm::ToolDefinition>,
        system: Option<SystemPrompt>,
    ) -> Result<(Vec<ContentBlock>, Option<StopReason>)> {
        // Get session ID
        let session_id = {
            let session = internals.session.read().await;
            session.session_id().to_string()
        };

        let mut stream = self
            .llm
            .stream_with_tools_and_system(
                messages,
                system,
                tools,
                None,
                self.config.thinking.clone(),
                Some(&session_id),
            )
            .await?;

        // Track content blocks as they're built
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut current_block_index: Option<usize> = None;
        let mut stop_reason: Option<StopReason> = None;

        // Track message metadata for logging
        let mut message_id: Option<String> = None;
        let mut model: Option<String> = None;
        let mut initial_usage: Option<omega_llm::Usage> = None;
        let mut output_tokens: u32 = 0;

        // Accumulators for building content blocks
        let mut text_accum = String::new();
        let mut thinking_accum = String::new();
        let mut thinking_signature = String::new();
        let mut tool_input_accum = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_signature: Option<String> = None;
        // Some providers send the full tool input upfront in
        // ContentBlockStart, unlike others which stream it incrementally
        // via InputJsonDelta.  We store it here as a fallback.
        let mut current_tool_start_input: Option<Value> = None;

        loop {
            tokio::select! {
                event_result = stream.next() => {
                    let event_result = match event_result {
                        Some(result) => result,
                        None => break, // Stream ended
                    };

                    match event_result {
                        Ok(event) => {
                            match event {
                                StreamEvent::MessageStart(msg_start) => {
                                    tracing::debug!("[StandardAgent] Stream started");
                                    // Capture message metadata for logging
                                    message_id = Some(msg_start.message.id.clone());
                                    model = Some(msg_start.message.model.clone());
                                    initial_usage = Some(msg_start.message.usage.clone());
                                }

                        StreamEvent::ContentBlockStart(block_start) => {
                            current_block_index = Some(block_start.index);

                            match &block_start.content_block {
                                ContentBlockStart::Text { .. } => {
                                    text_accum.clear();
                                }
                                ContentBlockStart::Thinking { .. } => {
                                    thinking_accum.clear();
                                    thinking_signature.clear();
                                }
                                ContentBlockStart::ToolUse { id, name, input, signature } => {
                                    tool_input_accum.clear();
                                    current_tool_id = id.clone();
                                    current_tool_name = name.clone();
                                    current_tool_signature = signature.clone();
                                    // Save the start input – it is the full input for
                                    // providers that send it upfront (OpenAI) but an
                                    // empty object for providers that stream it via
                                    // InputJsonDelta.
                                    current_tool_start_input = Some(input.clone());
                                }
                            }
                        }

                        StreamEvent::ContentBlockDelta(delta) => {
                            match &delta.delta {
                                ContentDelta::TextDelta { text } => {
                                    text_accum.push_str(text);
                                    // Stream text to output immediately
                                    internals.send_text(text);
                                }
                                ContentDelta::ThinkingDelta { thinking } => {
                                    thinking_accum.push_str(thinking);
                                    // Stream thinking to output immediately
                                    internals.send_thinking(thinking);
                                }
                                ContentDelta::SignatureDelta { signature } => {
                                    thinking_signature.push_str(signature);
                                }
                                ContentDelta::InputJsonDelta { partial_json } => {
                                    tool_input_accum.push_str(partial_json);
                                }
                            }
                        }

                        StreamEvent::ContentBlockStop(block_stop) => {
                            if current_block_index == Some(block_stop.index) {
                                // Finalize the content block
                                if !text_accum.is_empty() {
                                    // Send text complete signal to CLI
                                    internals.send_text_complete(&text_accum);
                                    content_blocks.push(ContentBlock::Text {
                                        text: text_accum.clone(),
                                        cache_control: None,
                                    });
                                    text_accum.clear();
                                } else if !thinking_accum.is_empty() {
                                    // Send thinking complete signal to CLI
                                    internals.send_thinking_complete(&thinking_accum);
                                    content_blocks.push(ContentBlock::Thinking {
                                        thinking: thinking_accum.clone(),
                                        signature: thinking_signature.clone(),
                                    });
                                    thinking_accum.clear();
                                    thinking_signature.clear();
                                } else if !tool_input_accum.is_empty()
                                    || !current_tool_name.is_empty()
                                {
                                    // Determine the tool input:
                                    //   1. If we have accumulated InputJsonDelta events
                                    //      (incremental streaming), parse those.
                                    //   2. Otherwise, fall back to the input that was
                                    //      provided upfront in ContentBlockStart.
                                    //   3. If neither is available, use default.
                                    let input: Value = if !tool_input_accum.is_empty() {
                                        serde_json::from_str(&tool_input_accum).unwrap_or_default()
                                    } else if let Some(start_input) = current_tool_start_input.take() {
                                        start_input
                                    } else {
                                        Value::Null
                                    };
                                    content_blocks.push(ContentBlock::ToolUse {
                                        id: current_tool_id.clone(),
                                        name: current_tool_name.clone(),
                                        input,
                                        signature: current_tool_signature.clone(),
                                    });
                                    tool_input_accum.clear();
                                    current_tool_id.clear();
                                    current_tool_name.clear();
                                    current_tool_signature = None;
                                    current_tool_start_input = None;
                                }
                                current_block_index = None;
                            }
                        }

                        StreamEvent::MessageDelta(msg_delta) => {
                            stop_reason = msg_delta.delta.stop_reason;
                            // Capture final output tokens
                            output_tokens = msg_delta.usage.output_tokens;
                            // Prefer full usage from MessageDelta if available
                            // (Bug B fix: OpenAI sends input+cache tokens only in the
                            // final chunk, not in MessageStart.)
                            if msg_delta.usage.input_tokens.is_some() {
                                initial_usage = Some(omega_llm::Usage {
                                    input_tokens: msg_delta.usage.input_tokens.unwrap_or(0),
                                    output_tokens: msg_delta.usage.output_tokens,
                                    cache_creation_input_tokens: msg_delta.usage.cache_creation_input_tokens,
                                    cache_read_input_tokens: msg_delta.usage.cache_read_input_tokens,
                                    thoughts_token_count: None,
                                });
                            }
                        }

                        StreamEvent::MessageStop => {
                            tracing::debug!("[StandardAgent] Stream complete");
                        }

                        StreamEvent::Ping => {
                            tracing::trace!("[StandardAgent] Ping");
                        }

                        StreamEvent::Error(err) => {
                            tracing::error!(
                                "[StandardAgent] Stream error: {}: {}",
                                err.error.error_type,
                                err.error.message
                            );
                            internals.send_error(format!(
                                "Stream error: {}",
                                err.error.message
                            ));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("[StandardAgent] Stream error: {}", e);
                    return Err(e);
                }
            }
                }

                // Check for interrupt messages
                msg = internals.receive() => {
                    if let Some(InputMessage::Interrupt) = msg {
                        tracing::info!("[StandardAgent] Interrupt received");

                        // Finalize any in-progress text content block
                        if !text_accum.is_empty() {
                            content_blocks.push(ContentBlock::Text {
                                text: text_accum.clone(),
                                cache_control: None,
                            });
                        }
                        // Discard incomplete thinking blocks (signature may be incomplete)
                        // Discard partial tool calls (don't add them to content_blocks)

                        // Remove all ToolUse blocks from content_blocks (discard all tool calls)
                        content_blocks.retain(|block| !matches!(block, ContentBlock::ToolUse { .. }));

                        // Append interrupt notification to the assistant's content blocks
                        content_blocks.push(ContentBlock::Text {
                            text: "<vibe-working-agent-system>User interrupted this message</vibe-working-agent-system>".to_string(),
                            cache_control: None,
                        });

                        // Force a natural end-of-turn stop reason so the outer
                        // loop breaks instead of re-invoking the LLM (which
                        // would happen if stop_reason was left as ToolUse).
                        stop_reason = Some(StopReason::EndTurn);

                        break;
                    }
                }
            }
        }

        // Log the assembled response if debugger is enabled
        if let Some(debugger) = internals.context.get_resource::<Debugger>() {
            // Construct a response object similar to MessageResponse for logging
            let mut response_for_logging = serde_json::json!({
                "id": message_id.unwrap_or_else(|| "unknown".to_string()),
                "type": "message",
                "role": "assistant",
                "content": content_blocks,
                "model": model.unwrap_or_else(|| "streamed".to_string()),
                "stop_reason": stop_reason,
            });

            // Add usage information if we captured it
            if let Some(ref usage) = initial_usage {
                let usage_obj = serde_json::json!({
                    "input_tokens": usage.input_tokens,
                    "output_tokens": output_tokens,
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                });
                response_for_logging["usage"] = usage_obj;
            }

            if let Err(e) = debugger.log_api_response(&response_for_logging) {
                tracing::warn!(
                    "[StandardAgent] Failed to log streaming API response: {}",
                    e
                );
            }
        }

        // Emit cache telemetry for this LLM call
        if let Some(ref usage) = initial_usage {
            let telemetry = omega_core::core::CacheTelemetry {
                input_tokens: usage.input_tokens,
                output_tokens,
                cache_read_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
            };
            internals.send_cache_telemetry(telemetry);
        }

        Ok((content_blocks, stop_reason))
    }
}

/// Stamp cache_control on the last content block of a message.
/// Returns true if a marker was placed.
fn stamp_message_end(marker: &CacheControl, msg: &mut Message) -> bool {
    match &mut msg.content {
        omega_llm::MessageContent::Text(text) => {
            if text.is_empty() {
                return false;
            }
            msg.content = omega_llm::MessageContent::Blocks(vec![ContentBlock::Text {
                text: text.clone(),
                cache_control: Some(marker.clone()),
            }]);
            true
        }
        omega_llm::MessageContent::Blocks(blocks) => {
            if let Some(last) = blocks.last_mut() {
                let had_content = match last {
                    ContentBlock::Text { text, .. } => !text.is_empty(),
                    _ => true,
                };
                if had_content {
                    *last = last.clone().with_cache_control(marker.clone());
                    return true;
                }
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omega_llm::{CacheControl, ContentBlock, MessageContent};

    // -----------------------------------------------------------------------
    // stamp_message_end tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_stamp_message_end_on_text_content() {
        let marker = CacheControl::ephemeral_1h();
        let mut msg = Message::user("Hello world");

        let result = stamp_message_end(&marker, &mut msg);

        assert!(result);
        // Should have converted from Text to Blocks
        match &msg.content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::Text {
                        text,
                        cache_control,
                    } => {
                        assert_eq!(text, "Hello world");
                        assert!(cache_control.is_some());
                        assert_eq!(cache_control.as_ref().unwrap().cache_type, "ephemeral");
                    }
                    _ => panic!("Expected Text block"),
                }
            }
            _ => panic!("Expected Blocks content after stamp"),
        }
    }

    #[test]
    fn test_stamp_message_end_on_empty_text_returns_false() {
        let marker = CacheControl::ephemeral();
        let mut msg = Message::user("");

        let result = stamp_message_end(&marker, &mut msg);

        assert!(!result);
        // Message should remain as Text (not converted to Blocks)
        assert!(matches!(msg.content, MessageContent::Text(s) if s.is_empty()));
    }

    #[test]
    fn test_stamp_message_end_on_blocks() {
        let marker = CacheControl::ephemeral_1h();
        let mut msg = Message::user_with_blocks(vec![
            ContentBlock::text("Part 1"),
            ContentBlock::text("Part 2"),
        ]);

        let result = stamp_message_end(&marker, &mut msg);

        assert!(result);
        match &msg.content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                // First block unchanged
                assert!(blocks[0].as_text() == Some("Part 1"));
                // Last block should have cache_control
                match &blocks[1] {
                    ContentBlock::Text {
                        text,
                        cache_control,
                    } => {
                        assert_eq!(text, "Part 2");
                        assert!(cache_control.is_some());
                    }
                    _ => panic!("Expected Text block at position 1"),
                }
            }
            _ => panic!("Expected Blocks"),
        }
    }

    #[test]
    fn test_stamp_message_end_does_not_double_stamp() {
        let marker = CacheControl::ephemeral_1h();
        let other_marker = CacheControl::ephemeral_5m();
        let mut msg = Message::user_with_blocks(vec![ContentBlock::text_with_cache(
            "Already cached",
            other_marker,
        )]);

        // First stamp succeeds
        let result1 = stamp_message_end(&marker, &mut msg);
        assert!(result1);

        // Second stamp should still succeed (stamp_message_end doesn't check
        // for existing markers — it just re-stamps). This is by design:
        // apply_cache_control strips all markers first, then re-stamps.
        let result2 = stamp_message_end(&marker, &mut msg);
        assert!(result2);
    }
}

#[cfg(test)]
mod interrupt_tests {
    use super::*;
// ---------------------------------------------------------------------------
// Steering interrupt — agent must survive an idle Interrupt and keep working
// ---------------------------------------------------------------------------

/// A canned LLM provider for loop tests: always streams a single short
/// text reply and stops with EndTurn.
struct MockProvider;

use std::pin::Pin;

impl MockProvider {
    fn events() -> Vec<Result<omega_llm::StreamEvent>> {
        use omega_llm::{
            ContentBlockStart, ContentDelta, DeltaUsage, MessageDeltaData, MessageDeltaEvent,
            MessageStartData, MessageStartEvent, StopReason, Usage,
        };
        vec![
            Ok(omega_llm::StreamEvent::MessageStart(MessageStartEvent {
                message: MessageStartData {
                    id: "mock-1".into(),
                    message_type: "message".into(),
                    role: "assistant".into(),
                    content: vec![],
                    model: "mock-model".into(),
                    stop_reason: None,
                    stop_sequence: None,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        thoughts_token_count: None,
                    },
                },
            })),
            Ok(omega_llm::StreamEvent::ContentBlockStart(
                omega_llm::ContentBlockStartEvent {
                    index: 0,
                    content_block: ContentBlockStart::Text { text: String::new() },
                },
            )),
            Ok(omega_llm::StreamEvent::ContentBlockDelta(
                omega_llm::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentDelta::TextDelta {
                        text: "mock reply".into(),
                    },
                },
            )),
            Ok(omega_llm::StreamEvent::ContentBlockStop(
                omega_llm::ContentBlockStopEvent { index: 0 },
            )),
            Ok(omega_llm::StreamEvent::MessageDelta(MessageDeltaEvent {
                delta: MessageDeltaData {
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: DeltaUsage {
                    output_tokens: 5,
                    input_tokens: Some(10),
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
            })),
            Ok(omega_llm::StreamEvent::MessageStop),
        ]
    }
}

#[async_trait::async_trait]
impl omega_llm::LlmProvider for MockProvider {
    async fn stream_with_tools_and_system(
        &self,
        _messages: Vec<Message>,
        _system: Option<omega_llm::SystemPrompt>,
        _tools: Vec<omega_llm::ToolDefinition>,
        _tool_choice: Option<omega_llm::ToolChoice>,
        _thinking: Option<omega_llm::ThinkingConfig>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Pin<Box<dyn futures::Stream<Item = Result<omega_llm::StreamEvent>> + Send>>>
    {
        Ok(Box::pin(futures::stream::iter(Self::events())))
    }

    fn model(&self) -> String {
        "mock-model".into()
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn create_variant(
        &self,
        _model: &str,
        _max_tokens: u32,
    ) -> std::sync::Arc<dyn omega_llm::LlmProvider> {
        std::sync::Arc::new(MockProvider)
    }
}

/// Collect output chunks until Done (with a hard timeout so a stuck agent
/// fails the test instead of hanging).
async fn drain_until_done(
    rx: &mut crate::runtime::channels::OutputReceiver,
    chunks: &mut Vec<omega_core::core::OutputChunk>,
) {
    use tokio::time::timeout;
    loop {
        match timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(chunk)) => {
                let is_done = matches!(chunk, omega_core::core::OutputChunk::Done);
                chunks.push(chunk);
                if is_done {
                    return;
                }
            }
            Ok(Err(_)) => panic!("output channel closed before Done"),
            Err(_) => panic!("timed out waiting for Done"),
        }
    }
}

/// Regression test: an Interrupt received while the agent is idle (between
/// turns — e.g. the user pressed Esc after the turn already finished) must
/// NOT kill the agent. The agent must still accept and answer the next
/// user message (the steering flow).
#[tokio::test]
async fn idle_interrupt_does_not_kill_agent() {
    use omega_core::core::InputMessage;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = crate::session::SessionStorage::with_dir(temp.path());
    let session = crate::session::AgentSession::new_with_storage(
        "steer-test",
        "picrust",
        "Picrust",
        "test",
        "You are helpful.",
        storage,
    )
    .unwrap();

    let config = AgentConfig::new();
    let agent = StandardAgent::new(config, std::sync::Arc::new(MockProvider));
    let runtime = crate::runtime::AgentRuntime::new();
    let handle = runtime.spawn(session, |internals| agent.run(internals)).await.unwrap();

    // Turn 1: normal user input → mock reply → Done.
    handle.send_input("hello").await.unwrap();
    let mut rx = handle.subscribe();
    let mut chunks = Vec::new();
    drain_until_done(&mut rx, &mut chunks).await;
    assert!(
        chunks.iter().any(|c| matches!(c, omega_core::core::OutputChunk::TextDelta(t) if t == "mock reply")),
        "turn 1 must produce the mock reply"
    );

    // Esc pressed while idle: an Interrupt with no active turn.
    handle.send(InputMessage::Interrupt).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Turn 2: the steering message. The agent must STILL be alive.
    handle.send_input("steer: different direction").await.unwrap();
    let mut chunks2 = Vec::new();
    drain_until_done(&mut rx, &mut chunks2).await;
    assert!(
        chunks2
            .iter()
            .any(|c| matches!(c, omega_core::core::OutputChunk::TextDelta(t) if t == "mock reply")),
        "agent must answer the steering message after an idle interrupt"
    );

    // Clean shutdown.
    handle.shutdown().await.unwrap();
}
}
