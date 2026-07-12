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
    llm::{AuthConfig, OpenAIProvider},
    omega_client::{
        proxy::{BashProxy, EditProxy, GlobProxy, GrepProxy, ReadProxy, WriteProxy},
        OmegaClient,
    },
    runtime::AgentRuntime,
    session::{AgentSession, SessionStorage},
    tools::{AskUserQuestionTool, ToolRegistry},
};

/// System prompt for the agent
const SYSTEM_PROMPT: &str = r#"You are a helpful coding assistant with access to tools.

You have the following tools available:
- Read: Read file contents
- Write: Write or create files
- Bash: Execute shell commands

When the user asks you to do something, use the appropriate tools.
Be concise in your responses."#;

/// Create the tool registry with all available tools.
///
/// Read, Write, Edit, Bash, Glob, and Grep are proxied through the omega-sh
/// daemon.  AskUserQuestion runs in-process.
fn create_registry(session_id: &str, cwd: &str) -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    let omega = OmegaClient::new()
        .with_session(session_id)
        .with_dir(cwd);

    registry.register(ReadProxy::new(omega.clone()));
    registry.register(WriteProxy::new(omega.clone()));
    registry.register(EditProxy::new(omega.clone()));
    registry.register(BashProxy::new(omega.clone()));
    registry.register(GlobProxy::new(omega.clone()));
    registry.register(GrepProxy::new(omega.clone()));
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
    println!("A coding agent. All tools are allowed.");
    println!("Use --stream/-s flag to enable streaming responses.");
    println!("Use --think/-t flag to enable extended thinking.");
    println!("Prompt caching is enabled by default (use --no-cache to disable).\n");

    // --- Step 1: Create LLM provider with dynamic auth ---
    println!("[Setup] Creating LLM provider...");

    let llm = Arc::new(
        OpenAIProvider::with_auth_provider(|| async {
            let api_key = env::var("OPENAI_API_KEY")
                .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY environment variable not set"))?;

            let base_url = env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".to_string());

            Ok(AuthConfig::with_base_url(api_key, base_url))
        })
        .with_model(
            env::var("OPENAI_MODEL")
                .unwrap_or_else(|_| "gpt-4o".to_string()),
        )
        .with_max_tokens(16384),
    );
    println!("[Setup] Model: {}", llm.model());

    // --- Step 2: Create runtime ---
    let runtime = AgentRuntime::new();
    println!("[Setup] Runtime created");

    let cwd = std::env::current_dir()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|_| "?".to_string());

    // --- Step 3: Create tool registry ---
    let tools = Arc::new(create_registry(&session_id, &cwd)?);
    println!("[Setup] Tools registered: {:?}", tools.tool_names());

    // --- Step 4: Create hooks (none — all tools allowed) ---
    let hooks = picrust::hooks::HookRegistry::new();

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
        "[Setup] AgentConfig created{}{}{}",
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
    println!("Type your requests below. All tools are allowed.");
    if caching {
        println!("📋 Attempting to optimize context (OpenAI does not support Anthropic-style caching)");
    } else {
        println!(" Prompt caching disabled.");
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
