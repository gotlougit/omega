//! Picrust — A coding agent framework
//!
//! Run with:
//!   cargo run                     # New session (with caching)
//!   cargo run -- --resume         # Resume existing session
//!   cargo run -- --stream         # New session with streaming
//!   cargo run -- --stream --resume # Resume with streaming
//!   cargo run -- --think          # Enable extended thinking
//!   cargo run -- --stream --think # Streaming with thinking
//!   cargo run -- --no-cache       # Disable prompt caching

use std::env;
use std::sync::Arc;

use anyhow::{bail, Result};
use picrust::{
    agent::{AgentConfig, StandardAgent},
    cli::ConsoleRenderer,
    hooks::{HookContext, HookEvent, HookRegistry, HookResult},
    llm::{AnthropicProvider, AuthConfig},
    runtime::AgentRuntime,
    session::{AgentSession, SessionStorage},
    tools::{
        AskUserQuestionTool, BashTool, EditTool, GlobTool, GrepTool, ReadTool, ToolRegistry,
        WriteTool,
    },
};

/// System prompt for the agent
const SYSTEM_PROMPT: &str = r#"You are a helpful coding assistant with access to tools.

You have the following tools available:
- Read: Read file contents
- Write: Write or create files
- Bash: Execute shell commands

When the user asks you to do something, use the appropriate tools.
Be concise in your responses."#;

/// Create the tool registry with all available tools
fn create_registry() -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();

    registry.register(ReadTool::new()?);
    registry.register(WriteTool::new()?);
    registry.register(BashTool::new()?);
    registry.register(GrepTool::new()?);
    registry.register(GlobTool::new()?);
    registry.register(EditTool::new()?);
    registry.register(AskUserQuestionTool::new());

    Ok(registry)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("picrust=warn")
        .init();

    // Parse command line arguments
    let args: Vec<String> = env::args().collect();
    let resume = args.iter().any(|a| a == "--resume" || a == "-r");

    // Generate session ID with timestamp
    let session_id = format!(
        "picrust-session-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );

    println!("=== Picrust ===");
    println!("A coding agent powered by Claude.");
    println!("Read operations are pre-allowed. Others will require permission.");
    println!("Use --stream/-s flag to enable streaming responses.");
    println!("Use --think/-t flag to enable extended thinking.");
    println!("Prompt caching is enabled by default (use --no-cache to disable).\n");

    // --- Step 1: Create LLM provider with dynamic auth ---
    println!("[Setup] Creating LLM provider...");

    let llm = Arc::new(
        AnthropicProvider::with_auth_provider(|| async {
            let api_key = env::var("ANTHROPIC_KEY")
                .map_err(|_| anyhow::anyhow!("ANTHROPIC_KEY environment variable not set"))?;

            Ok(AuthConfig::with_base_url(
                api_key,
                "https://api.anthropic.com/v1/messages",
            ))
        })
        .with_model(
            env::var("ANTHROPIC_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string()),
        )
        .with_max_tokens(32000),
    );
    println!("[Setup] Model: {}", llm.model());

    // --- Step 2: Create runtime with global Read permission ---
    let runtime = AgentRuntime::new();
    runtime.global_permissions();
    println!("[Setup] Runtime created (Read tool globally allowed)");

    // --- Step 3: Create tool registry ---
    let tools = Arc::new(create_registry()?);
    println!("[Setup] Tools registered: {:?}", tools.tool_names());

    // --- Step 4: Create hooks ---
    let mut hooks = HookRegistry::new();

    // Block dangerous Bash commands
    hooks
        .add_with_pattern(HookEvent::PreToolUse, "Bash", |ctx: &mut HookContext| {
            let cmd = ctx
                .tool_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if cmd.contains("rm ") {
                HookResult::deny("Dangerous command blocked by safety hook")
            } else {
                HookResult::none()
            }
        })
        .expect("Invalid regex pattern");

    // Auto-approve read-only tools
    hooks
        .add_with_pattern(
            HookEvent::PreToolUse,
            "^(Read|Glob|Grep)$",
            |_ctx: &mut HookContext| HookResult::allow(),
        )
        .expect("Invalid regex pattern");

    println!("[Setup] Hooks configured: dangerous command blocker, read-only auto-approve");

    // --- Step 5: Create or load session ---
    let storage = SessionStorage::with_dir("./sessions");
    let session = if resume {
        if !AgentSession::exists_with_storage(&session_id, &storage) {
            bail!(
                "Cannot resume: session '{}' does not exist. Run without --resume to create a new session.",
                session_id
            );
        }
        let session = AgentSession::load_with_storage(&session_id, storage)?;
        println!(
            "[Setup] Resumed session: {} ({} messages in history)",
            session.session_id(),
            session.history().len()
        );
        session
    } else {
        let session = AgentSession::new_with_storage(
            &session_id,
            "picrust",
            "Picrust Agent",
            "A coding agent powered by Claude",
            SYSTEM_PROMPT,
            storage,
        )?;
        println!("[Setup] New session: {}", session.session_id());
        session
    };

    // --- Step 6: Configure the agent ---
    let streaming = args.iter().any(|a| a == "--stream" || a == "-s");
    let thinking = args.iter().any(|a| a == "--think" || a == "-t");
    let no_cache = args.iter().any(|a| a == "--no-cache");
    let caching = !no_cache;

    let mut config = AgentConfig::new()
        .with_tools(tools)
        .with_hooks(hooks)
        .with_debug(true)
        .with_streaming(streaming)
        .with_prompt_caching(caching);

    if thinking {
        config = config.with_thinking(16000);
    }

    println!(
        "[Setup] AgentConfig created with debug logging, hooks{}{}{}",
        if streaming { ", streaming enabled" } else { "" },
        if thinking { ", extended thinking enabled" } else { "" },
        if caching { ", prompt caching enabled" } else { ", prompt caching disabled" }
    );

    // --- Step 7: Create and spawn the agent ---
    let agent = StandardAgent::new(config, llm);

    println!("[Setup] Spawning agent...");
    let handle = runtime
        .spawn(session, move |internals| agent.run(internals))
        .await;
    println!("[Setup] Agent spawned!");

    // --- Step 8: Run the console renderer ---
    println!("[Setup] Starting console renderer...");
    println!();
    println!("Type your requests below. Read/Glob/Grep are auto-approved by hooks.");
    if caching {
        println!("💰 Prompt caching enabled: 90% cost savings on repeated content!");
        println!("   (Tools, system prompt, and conversation history are automatically cached)");
    } else {
        println!(" Prompt caching disabled. To enable: run without --no-cache flag");
    }
    println!("Type 'exit' or 'quit' to stop.\n");

    let renderer = ConsoleRenderer::new(handle)
        .show_thinking(true)
        .show_tools(true);

    renderer.run().await?;

    // --- Cleanup ---
    println!("\n[Cleanup] Shutting down runtime...");
    runtime.shutdown_all().await;

    println!("[Cleanup] Done.");
    Ok(())
}
