//! Session transcript parsing + HTML rendering.
//!
//! `history.jsonl` is omega-loop's append-only message log; each line is a
//! `Message` with the same shape omega-llm writes (`role` + `content`, where
//! content is either a plain string or a list of typed blocks: text,
//! thinking, tool_use, tool_result, ...). We mirror just the fields we render.
//!
//! Rendering is server-side and escape-first; every piece of user/agent data
//! goes through `html_escape` before it can reach the page. Collapsing is done
//! with `<details>/<summary>` — no JS.

use serde::Deserialize;
use serde_json::Value;

use crate::templates::html_escape;

/// Cap on how much of a tool result (or any block) we render, to keep pages
/// bounded even when a tool dumps megabytes.
const MAX_BLOCK_RENDER: usize = 10_000;
/// Cap on messages rendered per session page.
pub const MAX_MESSAGES: usize = 500;

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<Block>),
}

/// Mirrors `omega_llm::ContentBlock` (serde tag = "type").
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text: String },
    Thinking {
        thinking: String,
        #[serde(default, rename = "signature")]
        _signature: Option<String>,
    },
    RedactedThinking {
        #[serde(rename = "data")]
        _data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<Value>,
        #[serde(default)]
        is_error: Option<bool>,
    },
    Image {
        #[serde(rename = "source")]
        _source: Value,
    },
    Document {
        #[serde(rename = "source")]
        _source: Value,
    },
}

/// Parse `history.jsonl`, skipping lines that are malformed or unknown.
pub fn parse_history(raw: &str) -> Vec<Message> {
    let mut messages = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Message>(line) {
            Ok(m) => messages.push(m),
            Err(e) => tracing::warn!(line = i + 1, error = %e, "skipping unparseable history line"),
        }
    }
    messages
}

/// Render a message's content to escaped HTML (role rendered by the template).
pub fn render_content_html(msg: &Message) -> String {
    match &msg.content {
        MessageContent::Text(t) => format!(
            "<div class=\"message-text\">{}</div>",
            html_escape(t)
        ),
        MessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                out.push_str(&render_block(block));
            }
            out
        }
    }
}

fn render_block(block: &Block) -> String {
    match block {
        Block::Text { text } => {
            format!("<div class=\"message-text\">{}</div>", html_escape(text))
        }
        Block::Thinking { thinking, .. } => format!(
            "<details class=\"message-thinking\"><summary>thinking</summary><pre>{}</pre></details>",
            html_escape(&truncate(thinking))
        ),
        Block::RedactedThinking { .. } => {
            "<details class=\"message-thinking\"><summary>redacted thinking</summary></details>"
                .to_string()
        }
        Block::ToolUse { id, name, input, .. } => format!(
            "<details class=\"message-tool\"><summary>tool: <code>{}</code> <small>({})</small></summary><pre>{}</pre></details>",
            html_escape(name),
            html_escape(id),
            html_escape(&truncate(&pretty_json(input)))
        ),
        Block::ToolResult { tool_use_id, content, is_error, .. } => {
            let label = if *is_error == Some(true) {
                format!("tool result (error) — {tool_use_id}")
            } else {
                format!("tool result — {tool_use_id}")
            };
            let body = match content {
                Some(Value::String(s)) => truncate(s),
                Some(v) => truncate(&pretty_json(v)),
                None => "(no output)".to_string(),
            };
            format!(
                "<details class=\"message-tool-result\"><summary>{label}</summary><pre>{}</pre></details>",
                html_escape(&body)
            )
        }
        Block::Image { .. } | Block::Document { .. } => {
            "<div class=\"message-note text-muted\">[attachment]</div>".to_string()
        }
    }
}

fn pretty_json(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn truncate(s: &str) -> String {
    let mut out: String = s.chars().take(MAX_BLOCK_RENDER).collect();
    if s.chars().count() > MAX_BLOCK_RENDER {
        out.push_str("\n… (truncated)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_one(line: &str) -> String {
        let msg = serde_json::from_str::<Message>(line).unwrap();
        render_content_html(&msg)
    }

    #[test]
    fn parses_plain_text_messages() {
        let html = render_one(r#"{"role":"user","content":"hello world"}"#);
        assert!(html.contains("hello world"));
        assert!(!html.contains("&lt;"));
    }

    #[test]
    fn escapes_user_content() {
        let html = render_one(r#"{"role":"user","content":"<script>alert(1)</script>"}"#);
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn parses_tool_blocks() {
        let line = r#"{
            "role":"assistant",
            "content":[
                {"type":"text","text":"let me check"},
                {"type":"tool_use","id":"call_1","name":"Bash","input":{"command":"ls"}},
                {"type":"tool_result","tool_use_id":"call_1","content":"file1\nfile2"}
            ]
        }"#;
        let html = render_one(line);
        assert!(html.contains("let me check"));
        assert!(html.contains("tool: <code>Bash</code>"));
        assert!(html.contains("tool result"));
        assert!(html.contains("file1"));
        // The input JSON must be pretty-printed and escaped.
        assert!(html.contains("&quot;command&quot;"));
    }

    #[test]
    fn parses_thinking_blocks() {
        let line = r#"{
            "role":"assistant",
            "content":[{"type":"thinking","thinking":"hmm","signature":"sig"}]
        }"#;
        let html = render_one(line);
        assert!(html.contains("<details class=\"message-thinking\">"));
        assert!(html.contains("hmm"));
    }

    #[test]
    fn truncates_long_tool_output() {
        let big = "x".repeat(MAX_BLOCK_RENDER + 100);
        let line = format!(
            r#"{{"role":"assistant","content":[{{"type":"tool_result","tool_use_id":"c","content":"{big}"}}]}}"#
        );
        let html = render_one(&line);
        assert!(html.contains("(truncated)"));
    }
}
