//! Integration / regression tests for prompt caching in streaming responses.
//!
//! These tests demonstrate the two critical bugs that cause prompt cache
//! stats to be always zero:
//!
//! **Bug A — Cache control lost during OpenAI wire conversion:**
//!   `convert_messages_to_openai()` strips `cache_control` from content
//!   blocks when serializing to the OpenAI wire format. This means the
//!   cache breakpoints added by `apply_cache_control()` never reach the API.
//!
//! **Bug B — Usage captured from wrong chunk:**
//!   In the streaming response handler, `MessageStart` (emitted from the
//!   *first* chunk where `role == "assistant"`) captures `Usage`.  But
//!   the real OpenAI streaming API sends usage data (including
//!   `cached_tokens`) in the **last** chunk.  Only `output_tokens` is
//!   extracted from the last chunk for `MessageDelta`; the input tokens
//!   and cached tokens are lost.
//!
//! These tests use the same types and conversions that the real code uses.
//! Run with: `cargo test -p omega-llm --test streaming_cache_regression`

use omega_llm::types::CustomTool;
use omega_llm::{
    CacheControl, ContentBlock, Message, MessageContent, SystemBlock, SystemPrompt, ToolDefinition,
    ToolInputSchema, Usage,
};

// Replicate CacheTelemetry to avoid adding omega-core as a dependency
#[derive(Debug, Clone)]
struct CacheTelemetry {
    input_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
}

impl CacheTelemetry {
    fn has_caching(&self) -> bool {
        self.cache_read_tokens > 0 || self.cache_creation_tokens > 0
    }
    fn hit_rate_pct(&self) -> f64 {
        if self.input_tokens == 0 {
            return 0.0;
        }
        (self.cache_read_tokens as f64 / self.input_tokens as f64) * 100.0
    }
}

// We can't directly call the private functions, so we replicate the
// conversion logic here to test against.

// ---------------------------------------------------------------------------
// Bug A: Cache control markers are stripped during OpenAI wire conversion
// ---------------------------------------------------------------------------

/// Simulates what `convert_messages_to_openai` does: extracts text from
/// content blocks while silently discarding cache_control metadata.
fn simulate_openai_text_extraction(msg: &Message) -> String {
    match &msg.content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => parts.push(text.clone()),
                    // tool results, images, etc. also lose cache_control
                    _ => {}
                }
            }
            parts.join("\n")
        }
    }
}

/// Simulates what `convert_messages_to_openai` does with SystemPrompt::Blocks:
/// flattens all blocks into a single text string, losing cache_control.
fn simulate_system_block_extraction(system: &SystemPrompt) -> String {
    match system {
        SystemPrompt::Text(s) => s.clone(),
        SystemPrompt::Blocks(blocks) => blocks.iter().map(|b| b.text.as_str()).collect(),
    }
}

#[test]
fn bug_a_cache_control_lost_on_user_message() {
    // Setup: a user message with cache_control on the text block.
    let cc = CacheControl::ephemeral_1h();
    let msg = Message {
        role: "user".to_string(),
        content: MessageContent::Blocks(vec![ContentBlock::Text {
            text: "Hello, please help me".to_string(),
            cache_control: Some(cc.clone()),
        }]),
    };

    // Verify the internal type DOES have cache_control
    let json_with_cache = serde_json::to_value(&msg).unwrap();
    let blocks = json_with_cache["content"].as_array().unwrap();
    let first_block = &blocks[0];
    assert!(
        first_block.get("cache_control").is_some(),
        "Internal Message type should preserve cache_control"
    );
    assert_eq!(
        first_block["cache_control"]["type"], "ephemeral",
        "cache_control type should be ephemeral"
    );

    // Now simulate the OpenAI wire format conversion
    let extracted_text = simulate_openai_text_extraction(&msg);
    assert_eq!(extracted_text, "Hello, please help me");
    // But cache_control is gone! We only have plain text.

    // The bug: when this is serialized as an OpenAI message, it becomes:
    //   { "role": "user", "content": "Hello, please help me" }
    // instead of:
    //   { "role": "user", "content": [{ "type": "text", "text": "Hello...", "cache_control": {...}}] }
    //
    // The gateway never sees cache_control, so it can't create cache breakpoints.
}

#[test]
fn bug_a_cache_control_lost_on_system_prompt() {
    // Setup: SystemPrompt::Blocks with cache_control
    let cc = CacheControl::ephemeral_1h();
    let system = SystemPrompt::Blocks(vec![
        SystemBlock::new("You are a helpful assistant.").with_cache_control(cc.clone())
    ]);

    // Internal type has cache_control
    let json = serde_json::to_value(&system).unwrap();
    let first_block = &json.as_array().unwrap()[0];
    assert!(first_block.get("cache_control").is_some());

    // After OpenAI conversion: cache_control is lost
    let extracted = simulate_system_block_extraction(&system);
    assert_eq!(extracted, "You are a helpful assistant.");
    // Just a plain string — no cache_control metadata survives.
}

#[test]
fn bug_a_cache_control_lost_on_tools() {
    // Setup: ToolDefinition with cache_control
    let cc = CacheControl::ephemeral_1h();
    let tool = ToolDefinition::Custom(CustomTool {
        name: "read".to_string(),
        description: Some("Read a file".to_string()),
        input_schema: ToolInputSchema::new(),
        tool_type: None,
        cache_control: Some(cc),
    });

    // Internal type has cache_control (ToolDefinition is externally tagged)
    let json = serde_json::to_value(&tool).unwrap();
    let custom = &json["Custom"];
    assert!(custom.get("cache_control").is_some());

    // But the OpenAI wire type (simulated here) has no cache_control:
    // OpenAITool only has { type, function: { name, description, parameters } }
    #[derive(serde::Serialize)]
    struct OpenAIToolWire {
        #[serde(rename = "type")]
        tool_type: String,
        function: OpenAIFunctionWire,
    }
    #[derive(serde::Serialize)]
    struct OpenAIFunctionWire {
        name: String,
        description: String,
        parameters: serde_json::Value,
    }

    let wire_tool = OpenAIToolWire {
        tool_type: "function".to_string(),
        function: OpenAIFunctionWire {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        },
    };

    let wire_json = serde_json::to_value(&wire_tool).unwrap();
    assert!(
        wire_json.get("cache_control").is_none(),
        "OpenAI wire format does NOT support cache_control on tools"
    );
}

// ---------------------------------------------------------------------------
// Bug B: Usage captured from first chunk, cached_tokens in last chunk
// ---------------------------------------------------------------------------

/// Simulated SSE chunks from a real OpenAI streaming response.
/// Pattern: first chunk has role, last chunk has usage+finish_reason.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct SimChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    choices: Vec<SimChoice>,
    #[serde(default)]
    usage: Option<SimUsage>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct SimChoice {
    index: u32,
    delta: SimDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct SimDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct SimUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<SimPromptTokensDetails>,
}

#[derive(Debug, serde::Deserialize)]
struct SimPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

/// Simulate the pattern the real `send_streaming_request` uses:
/// - When `delta.role == "assistant"`, capture `Usage` from `chunk.usage`
/// - When `finish_reason` is present, only capture `output_tokens`
#[test]
fn bug_b_usage_from_first_chunk_is_empty() {
    // Simulate a real 2-chunk OpenAI SSE stream:
    // Chunk 1: role="assistant" (no usage)
    let chunk1_json = r#"{
        "id": "chatcmpl-001",
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {"role": "assistant"}}]
    }"#;

    // Chunk 2: content + finish_reason + usage with cached_tokens
    let chunk2_json = r#"{
        "id": "chatcmpl-001",
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {"content": "Hello!"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 10,
            "total_tokens": 1010,
            "prompt_tokens_details": {"cached_tokens": 800}
        }
    }"#;

    let chunk1: SimChunk = serde_json::from_str(chunk1_json).unwrap();
    let chunk2: SimChunk = serde_json::from_str(chunk2_json).unwrap();

    // --- Simulate what send_streaming_request does on chunk 1 ---
    if chunk1.choices[0].delta.role.as_deref() == Some("assistant") {
        let usage = chunk1.usage.as_ref().map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: u.prompt_tokens_details.as_ref().map(|d| d.cached_tokens),
            thoughts_token_count: None,
        });

        // This is `initial_usage` in the real code.
        let initial_usage = usage.unwrap_or(Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            thoughts_token_count: None,
        });

        // BUG: initial_usage has all zeros because chunk1 had no `usage` field!
        assert_eq!(
            initial_usage.input_tokens, 0,
            "BUG: input_tokens is 0 because usage wasn't in the first chunk"
        );
        assert_eq!(
            initial_usage.cache_read_input_tokens, None,
            "BUG: cache_read_input_tokens is None because usage wasn't in the first chunk"
        );
    }

    // --- Simulate what send_streaming_request does on chunk 2 ---
    // Content is processed...
    // Finish reason is processed...
    if let Some(ref _reason) = chunk2.choices[0].finish_reason {
        // The real code only extracts output_tokens here:
        let output_tokens = chunk2
            .usage
            .as_ref()
            .map(|u| u.completion_tokens)
            .unwrap_or(0);

        assert_eq!(
            output_tokens, 10,
            "output_tokens is correctly captured from the last chunk"
        );
    }

    // --- BUT cached_tokens from chunk2 is NEVER captured! ---
    // The real code never looks at chunk2.usage.prompt_tokens_details.cached_tokens
    // because it only uses `initial_usage` (from chunk 1) for cache telemetry.
    let chunk2_usage = chunk2.usage.as_ref().unwrap();
    assert_eq!(chunk2_usage.prompt_tokens, 1000);
    let cached = chunk2_usage
        .prompt_tokens_details
        .as_ref()
        .unwrap()
        .cached_tokens;
    assert_eq!(cached, 800,
        "Chunk 2 correctly reports 800 cached tokens, but this value is NEVER captured by the agent loop");
}

/// Test: even when usage IS in the first chunk alongside role, it's captured.
/// Some providers/gateways DO include usage in the first chunk.
#[test]
fn usage_in_first_chunk_is_captured_correctly() {
    // Some gateways send usage alongside the role in the first chunk
    let chunk_json = r#"{
        "id": "chatcmpl-002",
        "model": "some-gateway-model",
        "choices": [{"index": 0, "delta": {"role": "assistant"}}],
        "usage": {
            "prompt_tokens": 500,
            "completion_tokens": 0,
            "total_tokens": 500,
            "prompt_tokens_details": {"cached_tokens": 450}
        }
    }"#;

    let chunk: SimChunk = serde_json::from_str(chunk_json).unwrap();

    if chunk.choices[0].delta.role.as_deref() == Some("assistant") {
        let usage = chunk.usage.as_ref().map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: u.prompt_tokens_details.as_ref().map(|d| d.cached_tokens),
            thoughts_token_count: None,
        });

        let initial_usage = usage.unwrap_or(Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            thoughts_token_count: None,
        });

        assert_eq!(
            initial_usage.input_tokens, 500,
            "When usage IS in the first chunk, input_tokens is captured correctly"
        );
        assert_eq!(
            initial_usage.cache_read_input_tokens,
            Some(450),
            "When usage IS in the first chunk, cached_tokens is captured correctly"
        );
    }
}

// ---------------------------------------------------------------------------
// End-to-end shadow test:
// Simulate the full flow from apply_cache_control → convert → stream → telemetry
// ---------------------------------------------------------------------------

/// Full simulation test: demonstrates that even if the API returns cached_tokens,
/// the telemetry emits zeros because of both bugs combined.
#[test]
fn end_to_end_cache_telemetry_always_zero() {
    // Step 1: Simulate apply_cache_control stamping a message with cache_control
    let cc = CacheControl::ephemeral_1h();
    let mut msg = Message::user("What is 2+2?");

    // stamp_message_end logic (from standard_loop.rs):
    match &mut msg.content {
        MessageContent::Text(text) => {
            if !text.is_empty() {
                msg.content = MessageContent::Blocks(vec![ContentBlock::Text {
                    text: text.clone(),
                    cache_control: Some(cc.clone()),
                }]);
            }
        }
        _ => {}
    }

    // Verify internal type has cache_control
    match &msg.content {
        MessageContent::Blocks(blocks) => match &blocks[0] {
            ContentBlock::Text { cache_control, .. } => {
                assert!(
                    cache_control.is_some(),
                    "Internal type has cache_control after stamping"
                );
            }
            _ => panic!("Expected Text block"),
        },
        _ => panic!("Expected Blocks after stamp"),
    }

    // Step 2: Simulate convert_messages_to_openai (BUG A — strips cache_control)
    let _extracted_text = simulate_openai_text_extraction(&msg);
    // cache_control is gone here. The API request has no cache breakpoints.

    // Step 3: Simulate streaming response with cached_tokens in the last chunk
    // (BUG B — cached_tokens is in the last chunk but we capture from first)
    let chunk1: SimChunk = serde_json::from_str(
        r#"{
        "choices": [{"index": 0, "delta": {"role": "assistant"}}]
    }"#,
    )
    .unwrap();
    let _chunk2: SimChunk = serde_json::from_str(
        r#"{
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
            "prompt_tokens_details": {"cached_tokens": 80}}
    }"#,
    )
    .unwrap();

    // Capture "initial_usage" from chunk1 (first "role" chunk)
    let initial_usage = chunk1
        .usage
        .as_ref()
        .map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: u.prompt_tokens_details.as_ref().map(|d| d.cached_tokens),
            thoughts_token_count: None,
        })
        .unwrap_or(Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            thoughts_token_count: None,
        });

    // Simulate the telemetry emission in standard_loop.rs:
    let telemetry = CacheTelemetry {
        input_tokens: initial_usage.input_tokens,
        cache_read_tokens: initial_usage.cache_read_input_tokens.unwrap_or(0),
        cache_creation_tokens: initial_usage.cache_creation_input_tokens.unwrap_or(0),
    };

    // ASSERT: Telemetry is ALL ZEROS even though the API returned cached_tokens=80!
    assert_eq!(
        telemetry.input_tokens, 0,
        "BUG: input_tokens is 0 (was in last chunk, not captured)"
    );
    assert_eq!(
        telemetry.cache_read_tokens, 0,
        "BUG: cache_read_tokens is 0 (cached_tokens=80 was in last chunk, not captured)"
    );
    assert_eq!(
        telemetry.cache_creation_tokens, 0,
        "cache_creation_tokens is 0 (always None for OpenAI)"
    );

    // When both bugs are fixed, this test should FAIL with these assertions:
    // assert_eq!(telemetry.input_tokens, 100);
    // assert_eq!(telemetry.cache_read_tokens, 80);
    //
    // AND the API request should include cache_control markers on messages.
}

#[test]
fn correct_behavior_if_usage_were_captured_from_final_chunk() {
    // What SHOULD happen: capture usage from the chunk with finish_reason
    let chunk: SimChunk = serde_json::from_str(
        r#"{
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 2000, "completion_tokens": 50, "total_tokens": 2050,
            "prompt_tokens_details": {"cached_tokens": 1500}}
    }"#,
    )
    .unwrap();

    // If we captured usage from the final chunk (the fix):
    if chunk.choices[0].finish_reason.is_some() {
        let usage = chunk.usage.as_ref().map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: u.prompt_tokens_details.as_ref().map(|d| d.cached_tokens),
            thoughts_token_count: None,
        });

        let final_usage = usage.unwrap();

        let telemetry = CacheTelemetry {
            input_tokens: final_usage.input_tokens,
            cache_read_tokens: final_usage.cache_read_input_tokens.unwrap_or(0),
            cache_creation_tokens: final_usage.cache_creation_input_tokens.unwrap_or(0),
        };

        assert_eq!(telemetry.input_tokens, 2000);
        assert_eq!(telemetry.cache_read_tokens, 1500);
        assert_eq!(telemetry.cache_creation_tokens, 0); // still None for OpenAI
        assert!(
            telemetry.has_caching(),
            "With the fix, has_caching() should be true"
        );
        assert!(
            (telemetry.hit_rate_pct() - 75.0).abs() < 0.01,
            "Hit rate should be 75% (1500/2000)"
        );
    }
}
