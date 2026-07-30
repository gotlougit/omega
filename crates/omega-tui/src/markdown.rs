//! Markdown rendering for omega-tui using termimad.
//!
//! Parses markdown text via termimad/minimad and converts it into
//! the [`StyledText`] format used by omega-tui's block-based output —
//! so the existing rendering pipeline (diff-based, scrollback-preserving)
//! handles all display and scrolling.

use cli::{Span, Style, StyledText};
use termimad::{
    self, crossterm::style::Attribute, CompositeKind, CompoundStyle, FmtComposite, FmtLine,
    FmtText, MadSkin,
};

// ---------------------------------------------------------------------------
// Style conversion: termimad → cli
// ---------------------------------------------------------------------------

/// Convert a termimad [`CompoundStyle`] into our [`cli::Style`].
fn convert_style(cs: &CompoundStyle) -> Style {
    let mut s = Style::default();
    if let Some(fg) = cs.get_fg() {
        s.fg = Some(fg);
    }
    if let Some(bg) = cs.get_bg() {
        s = s.bg(bg);
    }
    if cs.has_attr(Attribute::Bold) {
        s = s.bold();
    }
    if cs.has_attr(Attribute::Underlined) {
        s = s.underline();
    }
    if cs.has_attr(Attribute::Italic) {
        s = s.italic();
    }
    if cs.has_attr(Attribute::CrossedOut) {
        s.strikethrough = true;
    }
    s
}

// ---------------------------------------------------------------------------
// Prefix helpers for list items, quotes, headers
// ---------------------------------------------------------------------------

fn render_list_item_prefix(skin: &MadSkin, depth: u8, line_buf: &mut StyledText) {
    let indent_style = convert_style(&skin.paragraph.compound_style);
    let bullet_style = convert_style(skin.bullet.compound_style());
    let indent = "  ".repeat(depth as usize);
    line_buf.push(Span::new(indent, indent_style));
    line_buf.push(Span::new(
        format!("{} ", skin.bullet.get_char()),
        bullet_style,
    ));
}

fn render_ordered_list_prefix(skin: &MadSkin, level: u8, index: u32, line_buf: &mut StyledText) {
    let indent_style = convert_style(&skin.paragraph.compound_style);
    let indent = "  ".repeat(level as usize);
    line_buf.push(Span::new(indent, indent_style));
    let ois = skin.ordered_item_style(level);
    let num_style = convert_style(&ois.index_style);
    let label = format!("{}. ", index);
    line_buf.push(Span::new(label, num_style));
}

fn render_list_followup_prefix(skin: &MadSkin, depth: u8, line_buf: &mut StyledText) {
    let indent_style = convert_style(&skin.paragraph.compound_style);
    let indent = "  ".repeat((depth + 2) as usize);
    line_buf.push(Span::new(indent, indent_style));
}

fn render_ordered_followup_prefix(
    skin: &MadSkin,
    level: u8,
    index: u32,
    line_buf: &mut StyledText,
) {
    let indent_len = termimad::ordered_item_indent(level, index);
    let indent_style = convert_style(&skin.paragraph.compound_style);
    let indent = " ".repeat(indent_len);
    line_buf.push(Span::new(indent, indent_style));
}

fn render_quote_prefix(skin: &MadSkin, line_buf: &mut StyledText) {
    let quote_style = convert_style(skin.quote_mark.compound_style());
    line_buf.push(Span::new(
        format!("{} ", skin.quote_mark.get_char()),
        quote_style,
    ));
}

// ---------------------------------------------------------------------------
// Render a single FmtComposite into the line buffer
// ---------------------------------------------------------------------------

fn render_composite(skin: &MadSkin, fc: &FmtComposite<'_>, line_buf: &mut StyledText) {
    let ls = skin.line_style(fc.kind);

    // ── prefixes (list markers, quote marks, …) ───────────────────────
    match fc.kind {
        CompositeKind::ListItem(depth) => {
            render_list_item_prefix(skin, depth, line_buf);
        }
        CompositeKind::OrderedListItem { level, index } => {
            render_ordered_list_prefix(skin, level, index, line_buf);
        }
        CompositeKind::ListItemFollowUp(depth) => {
            render_list_followup_prefix(skin, depth, line_buf);
        }
        CompositeKind::OrderedListItemFollowUp { level, index } => {
            render_ordered_followup_prefix(skin, level, index, line_buf);
        }
        CompositeKind::Quote => {
            render_quote_prefix(skin, line_buf);
        }
        CompositeKind::Header(_) | CompositeKind::Code | CompositeKind::Paragraph => {}
    }

    // ── compounds (the actual text fragments) ──────────────────────────
    for compound in &fc.compounds {
        let compound_style = skin.compound_style(ls, compound);
        let style = convert_style(&compound_style);
        line_buf.push(Span::new(compound.src.to_string(), style));
    }
}

// ---------------------------------------------------------------------------
// Main public API
// ---------------------------------------------------------------------------

/// Render a markdown string into a [`StyledText`], wrapped to the given
/// terminal `width`.  Returns a single `StyledText` with `\n` separating
/// lines — the caller's block-layout engine handles soft-wrapping and
/// scrollback.
///
/// Uses a default [`MadSkin`] with dark-background colours.
pub fn render_markdown(md: &str, width: usize) -> StyledText {
    let skin = MadSkin::default_dark();
    render_markdown_with_skin(md, width, &skin)
}

/// Like [`render_markdown`] but accepts an explicit [`MadSkin`].
pub fn render_markdown_with_skin(md: &str, width: usize, skin: &MadSkin) -> StyledText {
    let fmt_text = FmtText::from(skin, md, Some(width));
    fmt_text_to_styled_text(&fmt_text, skin)
}

/// Convert a pre-formatted [`FmtText`] into a [`StyledText`] (one logical
/// line per markdown line, joined by `'\n'`).
pub fn fmt_text_to_styled_text(fmt_text: &FmtText<'_, '_>, skin: &MadSkin) -> StyledText {
    let mut result = StyledText::new();
    let mut is_first = true;

    for line in &fmt_text.lines {
        if !is_first {
            result.push(Span::new("\n", Style::default()));
        }
        is_first = false;

        match line {
            FmtLine::Normal(fc) => {
                render_composite(skin, fc, &mut result);
            }
            FmtLine::HorizontalRule => {
                let rule_style = convert_style(skin.horizontal_rule.compound_style());
                let rule_char = skin.horizontal_rule.get_char();
                // A horizontal rule fills the terminal width.
                let w = fmt_text.width.unwrap_or(80).min(120);
                let rule_line: String = std::iter::repeat(rule_char).take(w).collect();
                result.push(Span::new(rule_line, rule_style));
            }
            FmtLine::TableRow(row) => {
                let sep_style = convert_style(&skin.table.compound_style);
                let vbar = skin.table_border_chars.vertical;
                for (i, cell) in row.cells.iter().enumerate() {
                    if i > 0 {
                        result.push(Span::new(format!(" {} ", vbar), sep_style));
                    }
                    render_composite(skin, cell, &mut result);
                }
            }
            FmtLine::TableRule(rule) => {
                let rule_style = convert_style(&skin.table.compound_style);
                let tbc = skin.table_border_chars;
                for (i, w) in rule.widths.iter().enumerate() {
                    if i > 0 {
                        result.push(Span::new(tbc.cross.to_string(), rule_style));
                    }
                    let hbar: String = std::iter::repeat(tbc.horizontal).take(*w).collect();
                    result.push(Span::new(hbar, rule_style));
                }
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: join all span texts into one string.
    fn spans_to_string(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn basic_paragraph() {
        let st = render_markdown("Hello **world**!", 80);
        let spans = st.spans();
        assert!(!spans.is_empty(), "paragraph should produce spans");
        let combined = spans_to_string(spans);
        assert!(combined.contains("Hello"), "should contain paragraph text");
        assert!(combined.contains("world"), "should contain bold text");

        // The bold span should have the bold attribute.
        let bold_span = spans.iter().find(|s| s.text == "world");
        assert!(bold_span.is_some(), "bold text should be a separate span");
        if let Some(span) = bold_span {
            assert!(span.style.bold, "bold span should have bold style");
        }
    }

    #[test]
    fn header_renders() {
        let st = render_markdown("# Title", 80);
        let spans = st.spans();
        assert!(!spans.is_empty());
        let combined = spans_to_string(spans);
        assert!(combined.contains("Title"), "header text should appear");
    }

    #[test]
    fn list_items() {
        let st = render_markdown("- one\n- two", 80);
        let combined = spans_to_string(st.spans());
        assert!(
            combined.contains('•') || combined.contains('*'),
            "list should contain bullet character"
        );
        assert!(combined.contains("one"), "first item");
        assert!(combined.contains("two"), "second item");
    }

    #[test]
    fn inline_code() {
        let st = render_markdown("Use `foo()` here", 80);
        let spans = st.spans();
        let code_span = spans.iter().find(|s| s.text == "foo()");
        assert!(code_span.is_some(), "inline code should be a separate span");
    }

    #[test]
    fn code_block() {
        let st = render_markdown("```\nlet x = 1;\n```", 80);
        let combined = spans_to_string(st.spans());
        assert!(combined.contains("let"), "code block content should appear");
    }

    #[test]
    fn horizontal_rule() {
        let st = render_markdown("---", 80);
        let spans = st.spans();
        assert!(!spans.is_empty(), "HR should produce at least one span");
        let combined = spans_to_string(spans);
        assert!(combined.len() >= 3, "HR should be at least 3 chars wide");
        // The default MadSkin uses '―' for horizontal rules.
        assert!(
            combined.contains('―'),
            "HR should contain the default hr character"
        );
    }

    #[test]
    fn empty_input() {
        let st = render_markdown("", 80);
        assert!(st.is_empty() || st.spans().is_empty());
    }

    #[test]
    fn whitespace_only() {
        // Whitespace-only markdown should not crash.
        let _st = render_markdown("   \n\n  ", 80);
    }

    #[test]
    fn quote_line() {
        let st = render_markdown("> quoted text", 80);
        let combined = spans_to_string(st.spans());
        assert!(combined.contains("quoted"), "quote text should appear");
    }

    #[test]
    fn inline_style_combinations() {
        let st = render_markdown("plain **bold** *italic* ***both*** `code`", 80);
        let spans = st.spans();
        assert!(!spans.is_empty());

        // Bold
        assert!(spans.iter().any(|s| s.text == "bold" && s.style.bold));
        // Italic
        assert!(spans.iter().any(|s| s.text == "italic" && s.style.italic));
        // Bold+italic
        assert!(spans
            .iter()
            .any(|s| s.text == "both" && s.style.bold && s.style.italic));
        // Inline code
        assert!(spans.iter().any(|s| s.text == "code"));
    }

    #[test]
    fn nested_list() {
        let st = render_markdown("- a\n  - b\n    - c", 80);
        let combined = spans_to_string(st.spans());
        assert!(combined.contains('•'), "bullets should appear");
        assert!(combined.contains("a"), "first item");
        assert!(combined.contains("b"), "nested item");
        assert!(combined.contains("c"), "double-nested item");
    }
}
