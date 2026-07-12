//! # picrust — UI client
//!
//! Thin user-interface process that connects to the `agentd` daemon,
//! sends user messages, and renders streaming responses.

use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::Result;

use picrust::omega_loop_client::{AgentdClient, OutputChunk, ServerEvent, SessionConfig};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tool_input_preview(input: &serde_json::Value) -> String {
    input
        .get("command")
        .or_else(|| input.get("file_path"))
        .or_else(|| input.get("pattern"))
        .and_then(|v| v.as_str())
        .map(|s| {
            if s.len() > 80 {
                format!("{}…", &s[..80])
            } else {
                s.to_string()
            }
        })
        .unwrap_or_default()
}

fn chunk_to_one_liner(chunk: &OutputChunk) {
    match chunk {
        OutputChunk::TextDelta(text) => {
            print!("{}", text);
            io::stdout().flush().ok();
        }
        OutputChunk::ToolStart { name, input, .. } => {
            let preview = tool_input_preview(input);
            print!("\n  \x1b[33m⚡ {name}\x1b[0m {preview}");
            io::stdout().flush().ok();
        }
        OutputChunk::ToolEnd { result, .. } => {
            if result.is_error {
                print!("  \x1b[31m✗\x1b[0m");
            } else {
                print!("  \x1b[32m✓\x1b[0m");
            }
            println!();
        }
        OutputChunk::Error(e) => {
            println!("\x1b[31mError: {e}\x1b[0m");
        }
        OutputChunk::Done => {
            println!();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let session_id = format!(
        "picrust-session-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );

    // Parse flags
    let args: Vec<String> = std::env::args().collect();
    let use_stream = !args.iter().any(|a| a == "--no-stream" || a == "-n");
    let use_think = args.iter().any(|a| a == "--think" || a == "-t");
    let no_cache = args.iter().any(|a| a == "--no-cache");

    println!("=== Picrust ===");
    println!("Connecting to agentd...\n");

    let mut client = AgentdClient::connect().await?;
    println!("✓ Connected to omega-loop");

    let config = SessionConfig {
        stream: use_stream,
        think: use_think,
        no_cache,
    };

    // --- interaction loop ---
    let mut first = true;

    loop {
        // Read user input
        print!("\n> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_string();

        if input.is_empty() {
            continue;
        }

        if input == "exit" || input == "/exit" || input == "/quit" {
            println!("Goodbye!");
            break;
        }

        // Send to omega-loop
        client
            .send_run(&session_id, &input, &config)
            .await?;

        // Read and display events
        loop {
            match client.recv_event().await? {
                None => {
                    eprintln!("\n[omega-loop disconnected]");
                    return Ok(());
                }
                Some(ServerEvent::Created { session_name, .. }) => {
                    if first {
                        println!("Session: {session_name}");
                        first = false;
                    }
                }
                Some(ServerEvent::Chunk { chunk, .. }) => {
                    // Check for AskUserQuestion
                    if let OutputChunk::AskUserQuestion {
                        request_id,
                        questions,
                    } = &chunk
                    {
                        handle_ask_question(&mut client, &session_id, request_id, questions)
                            .await?;
                        continue;
                    }

                    // Check for terminal
                    let is_done = matches!(&chunk, OutputChunk::Done | OutputChunk::Error(_));

                    chunk_to_one_liner(&chunk);

                    if is_done {
                        break;
                    }
                }
                Some(ServerEvent::Unknown(val)) => {
                    tracing::debug!("Unknown server event: {val}");
                }
            }
        }
    }

    Ok(())
}

/// Handle an `AskUserQuestion` by printing the questions, collecting answers,
/// and sending the response back to the daemon.
async fn handle_ask_question(
    client: &mut AgentdClient,
    session_id: &str,
    request_id: &str,
    questions: &[picrust::omega_loop_client::UserQuestionWire],
) -> Result<()> {
    let mut answers = HashMap::new();

    for q in questions {
        println!("\n\x1b[36m{}\x1b[0m", q.header);
        println!("{}", q.question);

        for (i, opt) in q.options.iter().enumerate() {
            println!("  {}. {} — {}", i + 1, opt.label, opt.description);
        }

        print!("Answer (number): ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let choice = line.trim().parse::<usize>().ok().and_then(|n| {
            if n >= 1 && n <= q.options.len() {
                Some(q.options[n - 1].label.clone())
            } else {
                None
            }
        });

        if let Some(label) = choice {
            answers.insert(q.header.clone(), label);
        } else {
            answers.insert(q.header.clone(), String::new());
        }
    }

    client
        .send_ask_response(session_id, request_id, answers)
        .await?;

    Ok(())
}
