//! clankersh — direct shell into omega-sh daemon
//!
//! Connects to the running omega-sh daemon and lets you run Bash commands
//! (or other tools) interactively. Useful for debugging.
//!
//! ## Usage
//!
//! ```text
//! clankersh                          # interactive REPL
//! clankersh ls -la                   # run command and exit
//! clankersh Read /path/to/file       # use a specific tool
//! ```
//!
//! In REPL mode, lines starting with a tool name followed by `:` use that
//! tool (e.g. `Read: /tmp/foo`). Everything else is treated as Bash.

use std::io::{self, Write};

use anyhow::Result;
use omega_sh_client::{omega_core, OmegaClient};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let client = OmegaClient::new();

    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 {
        // One-shot mode: parse first arg as tool or default to Bash
        let (tool, cmd_args) = parse_tool_and_args(&args[1..]);
        match execute(&client, tool, &cmd_args).await {
            Ok(result) => {
                print_result(&result);
                std::process::exit(if result.is_error { 1 } else { 0 });
            }
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }

    // Interactive REPL
    println!("clankersh — omega-sh shell (type 'exit' to quit)");
    println!("Lines starting with ToolName: use that tool. Defaults to Bash.");
    println!();

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        if line == "exit" || line == "/exit" || line == "/quit" {
            break;
        }

        let args: Vec<&str> = line.split_whitespace().collect();
        let (tool, cmd_args) = parse_tool_and_args_slice(&args);
        match execute(&client, tool, &cmd_args).await {
            Ok(result) => print_result(&result),
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    Ok(())
}

fn parse_tool_and_args(args: &[String]) -> (&str, Vec<String>) {
    if args.is_empty() {
        return ("Bash", vec![]);
    }
    if let Some((first, rest)) = args.split_first() {
        // Check if first arg looks like a tool name (capitalized)
        if first.chars().next().is_some_and(|c| c.is_uppercase())
            && !first.contains('/')
            && !first.contains('.')
        {
            (first.as_str(), rest.to_vec())
        } else {
            ("Bash", args.to_vec())
        }
    } else {
        ("Bash", vec![])
    }
}

fn parse_tool_and_args_slice<'a>(args: &'a [&'a str]) -> (&'a str, Vec<&'a str>) {
    if args.is_empty() {
        return ("Bash", vec![]);
    }
    // Check for "ToolName: rest of line" format
    let first = args[0];
    if let Some(tool) = first.strip_suffix(':') {
        if tool.chars().next().is_some_and(|c| c.is_uppercase()) {
            return (tool, args[1..].to_vec());
        }
    }
    // Check if first word looks like a tool name (uppercase first letter, no slashes/dots)
    if first.chars().next().is_some_and(|c| c.is_uppercase())
        && !first.contains('/')
        && !first.contains('.')
    {
        (first, args[1..].to_vec())
    } else {
        ("Bash", args.to_vec())
    }
}

async fn execute(
    client: &OmegaClient,
    tool: &str,
    args: &[impl AsRef<str>],
) -> Result<omega_core::core::ToolResult, String> {
    let args: Vec<String> = args.iter().map(|a| a.as_ref().to_string()).collect();
    let input = match tool {
        "Bash" => json!({ "command": args.join(" ") }),
        "Read" => json!({ "file_path": args.first().map(|s| s.as_str()).unwrap_or("") }),
        "Write" => {
            let path = args.first().map(|s| s.as_str()).unwrap_or("");
            let content = args.get(1..).map(|s| s.join(" ")).unwrap_or_default();
            json!({ "file_path": path, "content": content })
        }

        _ => json!({ "command": args.join(" ") }), // fallback to bash
    };
    client.execute(tool, input).await
}

fn print_result(result: &omega_core::core::ToolResult) {
    if result.is_error {
        eprintln!("Error:");
    }
    match &result.content {
        omega_core::core::ToolResultData::Text(text) => {
            println!("{text}");
        }
        omega_core::core::ToolResultData::Image { media_type, .. } => {
            println!("[Image: {media_type}]");
        }
        omega_core::core::ToolResultData::Document {
            media_type,
            description,
            ..
        } => {
            println!("[{description} — {media_type}]");
        }
    }
}
