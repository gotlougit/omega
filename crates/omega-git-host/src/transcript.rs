//! Session transcript parsing, assembly, and HTML rendering.
//!
//! `history.jsonl` is omega-loop's append-only message log; each line is a
//! `Message` with the same shape omega-llm writes (`role` + `content`, where
//! content is either a plain string or a list of typed blocks: text,
//! thinking, tool_use, tool_result, ...). We mirror just the fields we render.
//!
//! ## Display model
//!
//! The raw history is a flat user/assistant ping-pong where tool results
//! arrive as separate `user` messages and thinking/tool-calls are their own
//! assistant blocks. That is great for replaying a session but poor for
//! reading one. [`assemble`] folds the raw stream into a conversation:
//!
//! * a **user message** is the actual user text;
//! * an **assistant message** is the markdown-rendered reply, with
//!   - thinking blocks folded into a collapsed `<details>` section,
//!   - `tool_use` + the matching `tool_result` folded into a collapsed
//!     "tools" section — "assistant called X and got a response", not a
//!     user/assistant exchange.
//!
//! Rendering is server-side and escape-first; every piece of user/agent data
//! goes through `html_escape` (or the markdown renderer, which is
//! escape-first too) before it can reach the page. Raw HTML in markdown is
//! shown as literal escaped text, and link destinations are scheme-checked,
//! so untrusted model output cannot inject markup. Collapsing is done with
//! `<details>/<summary>` — no JS.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::Deserialize;
use serde_json::Value;

use crate::templates::html_escape;

/// Cap on how much of a tool result (or any block) we render, to keep pages
/// bounded even when a tool dumps megabytes. Shared with the live chat SSE
/// stream, which sends the same truncated result to the browser.
pub(crate) const MAX_BLOCK_RENDER: usize = 10_000;
/// Cap on messages rendered per session page.
pub const MAX_MESSAGES: usize = 500;
/// Length of the one-line tool-result preview shown in the tools summary.
const TOOL_PREVIEW: usize = 140;

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

/// A tool call the assistant made, with its result attached (if the model
/// reported one). Rendered as one collapsed unit: "Bash → ok".
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub id: String,
    pub input: String,
    pub result: String,
    pub is_error: bool,
    /// One-line preview of the result for the summary line.
    pub preview: String,
}

/// One visible message in the rendered conversation.
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: String,
    /// Markdown-rendered body (escaped HTML) — the actual text content.
    pub body: String,
    /// Raw thinking traces, rendered as collapsed `<details>` blocks.
    pub thinking: Vec<String>,
    /// Tool calls, each with input + result.
    pub tools: Vec<ToolCall>,
}

impl DisplayMessage {
    fn new(role: &str) -> Self {
        DisplayMessage {
            role: role.to_string(),
            body: String::new(),
            thinking: Vec::new(),
            tools: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.body.is_empty() && self.thinking.is_empty() && self.tools.is_empty()
    }
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

/// Fold the raw message stream into the display conversation (see the module
/// docs). Tool results arriving as `user` messages are attached to the
/// assistant message that issued the call instead of being shown as a
/// separate user turn.
pub fn assemble(messages: &[Message]) -> Vec<DisplayMessage> {
    let mut out: Vec<DisplayMessage> = Vec::new();
    // The assistant message currently being built. Tool results (which arrive
    // as the *next* user message) attach to it before it is flushed.
    let mut pending: Option<DisplayMessage> = None;

    for msg in messages {
        match msg.role.as_str() {
            "assistant" => {
                flush(&mut out, &mut pending);
                pending = Some(build_assistant(msg));
            }
            _ => {
                if is_pure_tool_result(msg) {
                    attach_tool_results(&mut pending, msg);
                } else {
                    flush(&mut out, &mut pending);
                    let dm = build_user(msg);
                    if !dm.is_empty() {
                        out.push(dm);
                    }
                }
            }
        }
    }
    flush(&mut out, &mut pending);
    out
}

fn flush(out: &mut Vec<DisplayMessage>, pending: &mut Option<DisplayMessage>) {
    if let Some(dm) = pending.take() {
        if !dm.is_empty() {
            out.push(dm);
        }
    }
}

/// A user-role message that exists only to carry tool results (or is empty).
fn is_pure_tool_result(msg: &Message) -> bool {
    match &msg.content {
        MessageContent::Blocks(blocks) => {
            !blocks.is_empty() && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. }))
        }
        MessageContent::Text(t) => t.trim().is_empty(),
    }
}

fn build_assistant(msg: &Message) -> DisplayMessage {
    let mut dm = DisplayMessage::new("assistant");
    let mut text = Vec::new();
    match &msg.content {
        MessageContent::Text(t) => text.push(t.clone()),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    Block::Text { text: t } => text.push(t.clone()),
                    Block::Thinking { thinking, .. } => dm.thinking.push(thinking.clone()),
                    Block::RedactedThinking { .. } => {
                        dm.thinking.push("[redacted thinking]".to_string())
                    }
                    Block::ToolUse { id, name, input } => dm.tools.push(ToolCall {
                        name: name.clone(),
                        id: id.clone(),
                        input: pretty_json(input),
                        result: String::new(),
                        is_error: false,
                        preview: String::new(),
                    }),
                    Block::Image { .. } | Block::Document { .. } => {
                        text.push("[attachment]".to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    dm.body = markdown_to_html(&text.join("\n\n"));
    dm
}

fn build_user(msg: &Message) -> DisplayMessage {
    let mut dm = DisplayMessage::new("user");
    let mut text = Vec::new();
    match &msg.content {
        MessageContent::Text(t) => text.push(t.clone()),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                if let Block::Text { text: t } = block {
                    text.push(t.clone());
                }
            }
        }
    }
    dm.body = markdown_to_html(&text.join("\n\n"));
    dm
}

/// Attach `tool_result` blocks in a user message to the pending assistant's
/// tool calls, matched by `tool_use_id`.
fn attach_tool_results(pending: &mut Option<DisplayMessage>, msg: &Message) {
    let Some(dm) = pending.as_mut() else {
        return; // orphan tool result — nothing to attach it to
    };
    let MessageContent::Blocks(blocks) = &msg.content else {
        return;
    };
    for block in blocks {
        let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = block
        else {
            continue;
        };
        if let Some(tool) = dm.tools.iter_mut().find(|t| t.id == *tool_use_id) {
            tool.result = truncate(&render_result(content));
            tool.is_error = *is_error == Some(true);
            tool.preview = preview_of(&tool.result, *is_error == Some(true));
        }
    }
}

fn render_result(content: &Option<Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(v) => pretty_json(v),
        None => String::new(),
    }
}

/// One-line, whitespace-collapsed preview of a tool result.
///
/// `pub(crate)`: the live chat SSE stream uses it too, so the running tool
/// summary shown in the browser matches the persisted transcript exactly.
pub(crate) fn preview_of(result: &str, is_error: bool) -> String {
    let mut preview: String = result
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if preview.len() > TOOL_PREVIEW {
        preview.truncate(TOOL_PREVIEW);
        preview.push('…');
    }
    if preview.is_empty() {
        if is_error {
            preview.push_str("error");
        } else {
            preview.push_str("(no output)");
        }
    }
    preview
}

fn pretty_json(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Truncate `s` to at most [`MAX_BLOCK_RENDER`] chars, marking cuts.
///
/// `pub(crate)`: reused by the live chat SSE stream for tool results.
pub(crate) fn truncate(s: &str) -> String {
    let mut out: String = s.chars().take(MAX_BLOCK_RENDER).collect();
    if s.chars().count() > MAX_BLOCK_RENDER {
        out.push_str("\n… (truncated)");
    }
    out
}

// ---------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------

/// Render CommonMark (tables, strikethrough, task lists) to HTML.
///
/// Escape-first and XSS-safe:
/// * raw HTML (blocks and inline) is stripped, not passed through;
/// * link/image destinations are scheme-checked (only http/https/mailto and
///   scheme-less relative URLs survive);
/// * every text/attribute value goes through [`html_escape`].
pub fn markdown_to_html(src: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let mut out = String::with_capacity(src.len() + 64);
    for event in Parser::new_ext(src, options) {
        match event {
            Event::Start(tag) => start_tag(&mut out, &tag),
            Event::End(tag) => end_tag(&mut out, &tag),
            Event::Text(t) => out.push_str(&html_escape(&t)),
            Event::Code(t) => {
                out.push_str("<code>");
                out.push_str(&html_escape(&t));
                out.push_str("</code>");
            }
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push_str("<br>\n"),
            Event::Rule => out.push_str("<hr>\n"),
            Event::TaskListMarker(checked) => out.push_str(if checked {
                "<input type=\"checkbox\" disabled checked> "
            } else {
                "<input type=\"checkbox\" disabled> "
            }),
            // Raw HTML is never passed through — it is shown as literal
            // (escaped) text so nothing the model says silently vanishes.
            Event::Html(h) | Event::InlineHtml(h) => out.push_str(&html_escape(&h)),
            Event::FootnoteReference(name) => {
                out.push_str("<sup>");
                out.push_str(&html_escape(&name));
                out.push_str("</sup>");
            }
            _ => {}
        }
    }
    out
}

fn start_tag(out: &mut String, tag: &Tag) {
    match tag {
        Tag::Paragraph => out.push_str("<p>"),
        Tag::Heading { level, .. } => {
            let n = heading_num(*level);
            out.push_str(&format!("<h{n}>"));
        }
        Tag::BlockQuote(_) => out.push_str("<blockquote>\n"),
        Tag::CodeBlock(kind) => {
            let lang = match kind {
                CodeBlockKind::Fenced(l) => sanitize_code_lang(l),
                CodeBlockKind::Indented => String::new(),
            };
            if lang.is_empty() {
                out.push_str("<pre><code>");
            } else {
                out.push_str(&format!("<pre><code class=\"language-{lang}\">"));
            }
        }
        Tag::List(start) => out.push_str(if start.is_some() { "<ol>\n" } else { "<ul>\n" }),
        Tag::Item => out.push_str("<li>"),
        Tag::Emphasis => out.push_str("<em>"),
        Tag::Strong => out.push_str("<strong>"),
        Tag::Strikethrough => out.push_str("<del>"),
        Tag::Link { dest_url, title, .. } => {
            let url = sanitize_url(dest_url);
            out.push_str(&format!("<a href=\"{}\"", html_escape(&url)));
            if !title.is_empty() {
                out.push_str(&format!(" title=\"{}\"", html_escape(title)));
            }
            out.push('>');
        }
        Tag::Image { dest_url, title, .. } => {
            // Images become links to the (sanitized) URL; the alt text is
            // rendered as the link label by the enclosing parser events.
            let url = sanitize_url(dest_url);
            out.push_str(&format!(
                "<a href=\"{}\" title=\"{}\">",
                html_escape(&url),
                html_escape(if title.is_empty() { "[image]" } else { title })
            ));
        }
        Tag::Table(_) => out.push_str("<table>\n<tbody>\n"),
        Tag::TableHead => out.push_str("<thead>\n<tr>"),
        Tag::TableRow => out.push_str("<tr>"),
        Tag::TableCell => out.push_str("<td>"),
        _ => {}
    }
}

fn end_tag(out: &mut String, tag: &TagEnd) {
    match tag {
        TagEnd::Paragraph => out.push_str("</p>\n"),
        TagEnd::Heading(level) => {
            let n = heading_num(*level);
            out.push_str(&format!("</h{n}>\n"));
        }
        TagEnd::BlockQuote(_) => out.push_str("</blockquote>\n"),
        TagEnd::CodeBlock => out.push_str("</code></pre>\n"),
        TagEnd::List(ordered) => {
            out.push_str(if *ordered { "</ol>\n" } else { "</ul>\n" })
        }
        TagEnd::Item => out.push_str("</li>\n"),
        TagEnd::Emphasis => out.push_str("</em>"),
        TagEnd::Strong => out.push_str("</strong>"),
        TagEnd::Strikethrough => out.push_str("</del>"),
        TagEnd::Link | TagEnd::Image => out.push_str("</a>"),
        TagEnd::Table => out.push_str("</tbody>\n</table>\n"),
        TagEnd::TableHead => out.push_str("</tr>\n</thead>\n"),
        TagEnd::TableRow => out.push_str("</tr>\n"),
        TagEnd::TableCell => out.push_str("</td>"),
        _ => {}
    }
}

fn heading_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Drop dangerous schemes (javascript:, data:, file:, ...); keep http(s),
/// mailto, and scheme-less (relative) URLs.
fn sanitize_url(url: &str) -> String {
    let trimmed = url.trim();
    if let Some((scheme, _)) = trimmed.split_once(':') {
        let scheme = scheme.trim().to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
            return String::new();
        }
    }
    trimmed.to_string()
}

/// Only alphanumerics and `-_+` survive into a code-block language class.
fn sanitize_code_lang(lang: &str) -> String {
    lang.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assemble_one(history: &str) -> Vec<DisplayMessage> {
        assemble(&parse_history(history))
    }

    #[test]
    fn plain_text_messages() {
        let dms = assemble_one(
            "{\"role\":\"user\",\"content\":\"hello world\"}\n\
             {\"role\":\"assistant\",\"content\":\"hi there\"}\n",
        );
        assert_eq!(dms.len(), 2);
        assert_eq!(dms[0].role, "user");
        assert!(dms[0].body.contains("hello world"));
        assert_eq!(dms[1].role, "assistant");
        assert!(dms[1].body.contains("hi there"));
    }

    #[test]
    fn escapes_user_content() {
        let dms = assemble_one(r#"{"role":"user","content":"<script>alert(1)</script>"}"#);
        assert!(dms[0].body.contains("&lt;script&gt;"));
        assert!(!dms[0].body.contains("<script>"));
    }

    #[test]
    fn raw_html_is_escaped_from_markdown() {
        let dms = assemble_one(r#"{"role":"assistant","content":"**bold** <script>x</script>"}"#);
        assert!(dms[0].body.contains("<strong>bold</strong>"));
        // Raw HTML survives only as escaped literal text.
        assert!(
            dms[0].body.contains("&lt;script&gt;x&lt;/script&gt;"),
            "raw html must be escaped, got: {}",
            dms[0].body
        );
        assert!(!dms[0].body.contains("<script>"));
    }

    #[test]
    fn dangerous_links_are_sanitized() {
        let dms = assemble_one(r#"{"role":"assistant","content":"[x](javascript:alert(1)) [ok](https://a.b)"}"#);
        assert!(dms[0].body.contains("href=\"\""), "javascript: link must be dropped: {}", dms[0].body);
        assert!(dms[0].body.contains("href=\"https://a.b\""));
    }

    #[test]
    fn markdown_tables_and_code_blocks_render() {
        let dms = assemble_one(
            "{\"role\":\"assistant\",\"content\":\"|a|b|\\n|-|-|\\n|1|2|\\n\\n```rust\\nfn f() {}\\n```\"}",
        );
        assert!(dms[0].body.contains("<table>"), "{}", dms[0].body);
        assert!(
            dms[0].body.contains("<pre><code class=\"language-rust\">"),
            "{}",
            dms[0].body
        );
    }

    #[test]
    fn folds_tool_use_and_result_into_assistant_message() {
        let line = "{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"let me check\"},{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}]}";
        let result = "{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"call_1\",\"content\":\"file1\\nfile2\"}]}";
        let dms = assemble_one(&format!("{line}\n{result}\n"));

        // Exactly one visible message: the assistant one, with the tool folded in.
        assert_eq!(dms.len(), 1, "tool results must not become a user message");
        let dm = &dms[0];
        assert_eq!(dm.role, "assistant");
        assert!(dm.body.contains("let me check"));
        assert_eq!(dm.tools.len(), 1);
        let tool = &dm.tools[0];
        assert_eq!(tool.name, "Bash");
        assert_eq!(tool.id, "call_1");
        assert!(tool.input.contains("\"command\""), "input json: {}", tool.input);
        assert!(tool.result.contains("file1"));
        assert!(tool.preview.contains("file1"));
        assert!(!tool.is_error);
    }

    #[test]
    fn tool_error_is_marked() {
        let line = "{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"c\",\"name\":\"Bash\",\"input\":{\"command\":\"false\"}}]}";
        let result = "{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"c\",\"content\":\"boom\",\"is_error\":true}]}";
        let dms = assemble_one(&format!("{line}\n{result}\n"));
        assert_eq!(dms.len(), 1);
        assert!(dms[0].tools[0].is_error);
        assert!(dms[0].tools[0].preview.contains("boom"));
    }

    #[test]
    fn thinking_folds_into_assistant_message() {
        let line = "{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"hmm\",\"signature\":\"sig\"},{\"type\":\"text\",\"text\":\"answer\"}]}";
        let dms = assemble_one(line);
        assert_eq!(dms.len(), 1);
        assert_eq!(dms[0].thinking.len(), 1);
        assert_eq!(dms[0].thinking[0], "hmm");
        assert!(dms[0].body.contains("answer"));
    }

    #[test]
    fn truncates_long_tool_output() {
        let big = "x".repeat(MAX_BLOCK_RENDER + 100);
        let line =
            r#"{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"Bash","input":{"command":"y"}}]}"#;
        let result = format!(
            r#"{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"c","content":"{big}"}}]}}"#
        );
        let dms = assemble_one(&format!("{line}\n{result}\n"));
        assert!(dms[0].tools[0].result.contains("(truncated)"));
    }

    #[test]
    fn multiple_assistant_turns_stay_separate() {
        let dms = assemble_one(
            "{\"role\":\"user\",\"content\":\"q1\"}\n\
             {\"role\":\"assistant\",\"content\":\"a1\"}\n\
             {\"role\":\"user\",\"content\":\"q2\"}\n\
             {\"role\":\"assistant\",\"content\":\"a2\"}\n",
        );
        assert_eq!(dms.len(), 4);
        assert!(dms[1].body.contains("a1"));
        assert!(dms[3].body.contains("a2"));
    }

    #[test]
    fn lists_close_with_correct_tags() {
        let dms = assemble_one(
            r#"{"role":"assistant","content":"- a\n- b\n\n1. x\n2. y"}"#,
        );
        let body = &dms[0].body;
        assert!(body.contains("<ul>\n<li>a</li>"), "{}", body);
        assert!(body.contains("</ul>"), "{}", body);
        assert!(body.contains("<ol>\n<li>x</li>"), "{}", body);
        assert!(body.contains("</ol>"), "{}", body);
        assert!(!body.contains("</ol>\n<li>a"), "ul must not close with ol: {}", body);
    }

    #[test]
    fn orphan_tool_result_is_dropped() {
        let result = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"nope","content":"x"}]}"#;
        let dms = assemble_one(result);
        assert!(dms.is_empty(), "orphan tool result must not render");
    }
}
