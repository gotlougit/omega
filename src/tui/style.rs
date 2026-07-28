//! Styled text types for terminal rendering — copied from tau-term-screen.
//!
//! Content is represented as sequences of [`Span`]s, each pairing a
//! plain-text string with a [`Style`]. Display width is always
//! computable from the text alone — no ANSI escape codes are stored
//! in the data model.

use std::io::{self, Write};

pub use crossterm::style::Color;
use crossterm::style::{Attribute, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::QueueableCommand;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ---------------------------------------------------------------------------
// display_width / truncate helpers
// ---------------------------------------------------------------------------

pub(crate) fn is_line_break_grapheme(grapheme: &str) -> bool {
    matches!(grapheme, "\n" | "\r\n" | "\r")
}

fn screen_grapheme_width(grapheme: &str) -> usize {
    if is_line_break_grapheme(grapheme) {
        0
    } else if grapheme == "\t" || grapheme.chars().any(char::is_control) {
        1
    } else {
        UnicodeWidthStr::width(grapheme)
    }
}

/// Display width of a string in terminal columns, measured by grapheme cluster.
pub fn display_width(text: &str) -> usize {
    UnicodeSegmentation::graphemes(text, true)
        .map(screen_grapheme_width)
        .sum()
}

/// Returns a string that fits within `max_width` terminal columns, appending an
/// ellipsis when truncation is needed.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 1 {
        return "…".to_owned();
    }

    let mut out = String::new();
    let mut width = 0;
    let prefix_width = max_width - 1;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = screen_grapheme_width(grapheme);
        if prefix_width < width + grapheme_width {
            break;
        }
        width += grapheme_width;
        out.push_str(grapheme);
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

/// Visual attributes for a character cell.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
    pub strikethrough: bool,
}

impl Style {
    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }
    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------------

/// A terminal cell: one character, its visual style, and display width.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    pub width: usize,
}

impl Cell {
    pub fn new(ch: char, style: Style) -> Self {
        let ch = if ch == '\t' {
            ' '
        } else if ch.is_control() {
            '�'
        } else {
            ch
        };
        Self {
            ch,
            style,
            width: ch.width().unwrap_or(0),
        }
    }

    pub fn plain(ch: char) -> Self {
        Self::new(ch, Style::default())
    }

    pub fn normalized(self) -> Self {
        let ch = if self.ch == '\t' {
            ' '
        } else if self.ch.is_control() {
            '�'
        } else {
            self.ch
        };
        if ch == self.ch {
            self
        } else {
            Self {
                ch,
                style: self.style,
                width: ch.width().unwrap_or(0),
            }
        }
    }

    pub fn with_width(mut self, width: usize) -> Self {
        self.width = width;
        self
    }

    pub fn col_width(&self) -> usize {
        self.width
    }
}

// ---------------------------------------------------------------------------
// Span
// ---------------------------------------------------------------------------

/// A run of text with a uniform style.
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// StyledText
// ---------------------------------------------------------------------------

/// A sequence of styled spans representing rich text.
#[derive(Clone, Debug, Default)]
pub struct StyledText {
    spans: Vec<Span>,
}

impl StyledText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, span: Span) {
        self.spans.push(span);
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    pub fn char_count(&self) -> usize {
        let mut text = String::new();
        for span in &self.spans {
            text.push_str(&span.text);
        }
        display_width(&text)
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }

    pub fn to_string(&self) -> String {
        let mut out = String::new();
        for span in &self.spans {
            out.push_str(&span.text);
        }
        out
    }

    pub fn to_cells(&self) -> Vec<Cell> {
        let mut cells = Vec::new();
        visit_styled_graphemes(&self.spans, |grapheme, style| {
            if !is_line_break_grapheme(grapheme) {
                push_grapheme_cells(&mut cells, grapheme, style);
            }
        });
        cells
    }
}

impl From<&str> for StyledText {
    fn from(s: &str) -> Self {
        Self {
            spans: vec![Span::plain(s)],
        }
    }
}

impl From<String> for StyledText {
    fn from(s: String) -> Self {
        Self {
            spans: vec![Span::plain(s)],
        }
    }
}

impl From<Span> for StyledText {
    fn from(span: Span) -> Self {
        Self {
            spans: vec![span],
        }
    }
}

impl From<Vec<Span>> for StyledText {
    fn from(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}

// ---------------------------------------------------------------------------
// BlockId, Align, StyledBlock
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
}

#[derive(Clone, Debug)]
pub struct StyledBlock {
    pub content: StyledText,
    pub right_content: StyledText,
    pub bg: Option<Color>,
    pub align: Align,
    pub margin_left: u16,
    pub margin_right: u16,
}

impl StyledBlock {
    pub fn new(content: impl Into<StyledText>) -> Self {
        Self {
            content: content.into(),
            right_content: StyledText::new(),
            bg: None,
            align: Align::Left,
            margin_left: 0,
            margin_right: 0,
        }
    }

    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub fn margin_left(mut self, n: u16) -> Self {
        self.margin_left = n;
        self
    }
}

impl From<&str> for StyledBlock {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for StyledBlock {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<StyledText> for StyledBlock {
    fn from(text: StyledText) -> Self {
        Self::new(text)
    }
}

// ---------------------------------------------------------------------------
// Grapheme/cell traversal helpers
// ---------------------------------------------------------------------------

pub(crate) fn push_grapheme_cells(cells: &mut Vec<Cell>, grapheme: &str, style: Style) {
    if grapheme == "\t" {
        cells.push(Cell::new(' ', style));
        return;
    }
    if grapheme.chars().any(char::is_control) {
        cells.push(Cell::new('�', style));
        return;
    }
    let grapheme_width = screen_grapheme_width(grapheme);
    for (idx, ch) in grapheme.chars().enumerate() {
        let width = if idx == 0 { grapheme_width } else { 0 };
        cells.push(Cell::new(ch, style).with_width(width));
    }
}

pub(crate) fn visit_styled_graphemes(spans: &[Span], mut f: impl FnMut(&str, Style)) {
    let mut text = String::new();
    let mut char_styles = Vec::new();
    for span in spans {
        for ch in span.text.chars() {
            char_styles.push((text.len(), span.style));
            text.push(ch);
        }
    }

    let mut style_idx = 0;
    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(text.as_str(), true) {
        while style_idx + 1 < char_styles.len() && char_styles[style_idx + 1].0 <= byte {
            style_idx += 1;
        }
        let style = char_styles
            .get(style_idx)
            .map(|(_, style)| *style)
            .unwrap_or_default();
        f(grapheme, style);
    }
}

/// Terminal-column width of a cell slice.
pub fn cell_slice_cols(cells: &[Cell]) -> usize {
    cells.iter().map(|c| c.col_width()).sum()
}

// ---------------------------------------------------------------------------
// emit_styled_cells
// ---------------------------------------------------------------------------

/// Emits a sequence of styled cells to the writer, tracking style changes.
pub fn emit_styled_cells(w: &mut impl Write, cells: &[Cell]) -> io::Result<()> {
    let mut current = Style::default();

    for cell in cells {
        if cell.style != current {
            if current != Style::default() {
                w.queue(SetAttribute(Attribute::Reset))?;
            }
            if cell.style != Style::default() {
                apply_style(w, &cell.style)?;
            }
            current = cell.style;
        }
        w.queue(Print(cell.normalized().ch))?;
    }

    if current != Style::default() {
        w.queue(SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}

fn apply_style(w: &mut impl Write, style: &Style) -> io::Result<()> {
    if let Some(fg) = style.fg {
        w.queue(SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg {
        w.queue(SetBackgroundColor(bg))?;
    }
    if style.bold {
        w.queue(SetAttribute(Attribute::Bold))?;
    }
    if style.underline {
        w.queue(SetAttribute(Attribute::Underlined))?;
    }
    if style.italic {
        w.queue(SetAttribute(Attribute::Italic))?;
    }
    if style.strikethrough {
        w.queue(SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// layout_lines / layout_block
// ---------------------------------------------------------------------------

/// Splits styled content into physical terminal lines based on width.
pub fn layout_lines(content: &StyledText, width: usize, preserve_last_newline: bool) -> Vec<Vec<Cell>> {
    let width = width.max(1);

    let mut logical_lines: Vec<Vec<Cell>> = vec![Vec::new()];
    visit_styled_graphemes(content.spans(), |grapheme, style| {
        if is_line_break_grapheme(grapheme) {
            logical_lines.push(Vec::new());
        } else {
            let line = logical_lines
                .last_mut()
                .expect("logical_lines always has at least one entry");
            push_grapheme_cells(line, grapheme, style);
        }
    });

    if !preserve_last_newline
        && logical_lines.len() > 1
        && logical_lines.last().is_some_and(|l| l.is_empty())
    {
        logical_lines.pop();
    }

    let mut result: Vec<Vec<Cell>> = Vec::new();
    for line in logical_lines {
        if line.is_empty() {
            result.push(Vec::new());
        } else {
            let mut row = Vec::new();
            let mut skip_zero_width_suffix = false;
            let mut col = 0usize;
            for cell in line {
                let w = cell.col_width();
                if skip_zero_width_suffix && w == 0 {
                    continue;
                }
                skip_zero_width_suffix = false;
                if width < w {
                    if !row.is_empty() {
                        result.push(row);
                        row = Vec::new();
                    }
                    row.push(Cell::new('�', cell.style));
                    result.push(row);
                    row = Vec::new();
                    col = 0;
                    skip_zero_width_suffix = true;
                    continue;
                }
                if width < col + w && !row.is_empty() {
                    result.push(row);
                    row = Vec::new();
                    col = 0;
                }
                row.push(cell);
                col += w;
            }
            if !row.is_empty() {
                result.push(row);
            }
        }
    }

    if result.is_empty() {
        result.push(Vec::new());
    }

    result
}

/// Lays out a [`StyledBlock`] into physical terminal lines.
pub fn layout_block(block: &StyledBlock, width: usize) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let requested_ml = block.margin_left as usize;
    let requested_mr = block.margin_right as usize;
    let ml = requested_ml.min(width.saturating_sub(1));
    let remaining_after_ml = width.saturating_sub(ml);
    let mr = requested_mr.min(remaining_after_ml.saturating_sub(1));
    let content_width = width.saturating_sub(ml + mr).max(1);

    let mut content_lines = layout_lines(&block.content, content_width, false);
    if block.align == Align::Left && !block.right_content.is_empty() && content_lines.len() == 1 {
        let right_cells = block.right_content.to_cells();
        let left_cols = cell_slice_cols(&content_lines[0]);
        let right_cols = cell_slice_cols(&right_cells);
        if left_cols + 1 + right_cols <= content_width {
            let padding = content_width - left_cols - right_cols;
            content_lines[0].extend(std::iter::repeat_n(Cell::plain(' '), padding));
            content_lines[0].extend(right_cells);
        }
    }

    let fill_style = Style {
        bg: block.bg,
        ..Style::default()
    };
    let fill = Cell::new(' ', fill_style);

    content_lines
        .iter()
        .map(|line| {
            let mut row = Vec::with_capacity(width);
            row.extend(std::iter::repeat_n(Cell::plain(' '), ml));

            let cw = cell_slice_cols(line);
            let padding = content_width.saturating_sub(cw);
            match block.align {
                Align::Left => {
                    row.extend(line.iter().copied());
                    row.extend(std::iter::repeat_n(fill, padding));
                }
                Align::Center => {
                    let left = padding / 2;
                    let right = padding - left;
                    row.extend(std::iter::repeat_n(fill, left));
                    row.extend(line.iter().copied());
                    row.extend(std::iter::repeat_n(fill, right));
                }
            }

            row.extend(std::iter::repeat_n(Cell::plain(' '), mr));

            if let Some(bg) = block.bg {
                let content_end = row.len().saturating_sub(mr);
                for cell in &mut row[ml..content_end] {
                    if cell.style.bg.is_none() {
                        cell.style.bg = Some(bg);
                    }
                }
            }

            row
        })
        .collect()
}
