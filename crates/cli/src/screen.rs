//! Screen state tracker and renderer — copied from tau-term-screen.
//!
//! [`Screen`] maintains an "actual" buffer representing what is
//! currently on the terminal. Two rendering methods use it:
//!
//! - [`Screen::update()`] — **Path 1** (differential update): diffs the visible
//!   viewport against the actual buffer and queues only the escape sequences
//!   needed to update changed cells.
//! - [`Screen::render_scrolling()`] — **Path 2** (scrolling render): diffs the
//!   full content array, queues changed lines in order, and lets `\r\n` at the
//!   bottom edge push content into the terminal's scrollback buffer.
//!
//! Key design choices:
//! - Simple line model (`Vec<Vec<Cell>>`) — no soft-wrap tracking.
//! - Relative cursor movement only (`MoveUp`, `\r`, `\n`, `MoveToColumn`).
//! - `\n` for downward movement (scrolls at bottom edge, unlike `MoveDown`).

use std::io::{self, Write};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::style::Print;
use crossterm::terminal::{self, ClearType};
use crossterm::QueueableCommand;

use crate::style::{cell_slice_cols, emit_styled_cells, Cell};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalize_cell_lines(lines: &[Vec<Cell>]) -> Vec<Vec<Cell>> {
    lines
        .iter()
        .map(|line| line.iter().copied().map(Cell::normalized).collect())
        .collect()
}

/// Backtrack the diff common-prefix to a grapheme-cluster boundary so we
/// don't repaint in the middle of a wide-char + continuation sequence.
fn repaint_prefix_for_cluster_boundary(
    mut common_prefix: usize,
    actual: &[Cell],
    desired: &[Cell],
) -> usize {
    if common_prefix == actual.len() && common_prefix == desired.len() {
        return common_prefix;
    }
    while 0 < common_prefix {
        let next_is_continuation = actual
            .get(common_prefix)
            .is_some_and(|cell| cell.col_width() == 0)
            || desired
                .get(common_prefix)
                .is_some_and(|cell| cell.col_width() == 0);
        let prev_is_continuation = actual
            .get(common_prefix - 1)
            .is_some_and(|cell| cell.col_width() == 0)
            || desired
                .get(common_prefix - 1)
                .is_some_and(|cell| cell.col_width() == 0);
        if !next_is_continuation && !prev_is_continuation {
            break;
        }
        common_prefix -= 1;
    }
    common_prefix
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

pub struct Screen {
    /// What we believe is currently displayed on the terminal.
    lines: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    width: usize,
}

struct ChangedLineRange {
    /// First changed line, as an absolute index into the full scrolling
    /// content.
    first_line: usize,
    /// Last changed line, inclusive. This may be past `all_lines.len()` when
    /// old on-screen rows disappeared and must be cleared.
    last_line: usize,
}

impl Screen {
    pub fn new(width: usize) -> Self {
        Self {
            lines: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            width: width.max(1),
        }
    }

    pub fn set_width(&mut self, width: usize) {
        self.width = width.max(1);
    }

    pub fn width(&self) -> usize {
        self.width
    }

    /// Diffs desired content against actual screen state and queues only the
    /// escape sequences needed to make the terminal match.
    pub fn update(
        &mut self,
        w: &mut impl Write,
        desired_lines: &[Vec<Cell>],
        desired_cursor: (usize, usize),
    ) -> io::Result<()> {
        let desired_lines = normalize_cell_lines(desired_lines);

        if desired_lines.is_empty() {
            if !self.lines.is_empty() {
                self.move_to(w, 0, 0)?;
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            }
            self.lines.clear();
            self.cursor_row = 0;
            self.cursor_col = 0;
            return Ok(());
        }

        let desired_count = desired_lines.len();

        for (row, desired_line) in desired_lines.iter().enumerate() {
            let actual_line = self.lines.get(row);
            let actual_slice = actual_line.map(|l| l.as_slice()).unwrap_or(&[]);
            let desired_slice = desired_line.as_slice();

            let common_prefix = actual_slice
                .iter()
                .zip(desired_slice.iter())
                .take_while(|(a, d)| a == d)
                .count();
            let common_prefix =
                repaint_prefix_for_cluster_boundary(common_prefix, actual_slice, desired_slice);

            let is_last_desired = row == desired_count - 1;
            let actual_wider = cell_slice_cols(actual_slice) > cell_slice_cols(desired_slice);
            let has_extra_actual_below = is_last_desired && self.lines.len() > desired_count;

            if common_prefix == actual_slice.len()
                && common_prefix == desired_slice.len()
                && !has_extra_actual_below
            {
                continue;
            }

            let prefix_cols = cell_slice_cols(&desired_slice[..common_prefix]);
            self.move_to(w, row, prefix_cols)?;

            if common_prefix < desired_slice.len() {
                emit_styled_cells(w, &desired_slice[common_prefix..])?;
                self.cursor_col = cell_slice_cols(desired_slice);
            }

            if has_extra_actual_below {
                self.leave_pending_wrap_for_clear(w)?;
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            } else if actual_wider {
                w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            }
        }

        self.move_to(w, desired_cursor.0, desired_cursor.1)?;
        self.lines = desired_lines.to_vec();
        Ok(())
    }

    /// Renders all lines with scrolling support (Pi-style).
    ///
    /// Unlike `update()` which diffs only the visible viewport,
    /// this method diffs against the full previous content and
    /// queues changed lines in order. When rendering goes past
    /// the bottom of the terminal, `\r\n` naturally pushes the
    /// top row into the terminal's scrollback buffer.
    ///
    /// Call this instead of `update()` when the viewport top
    /// increased (content overflowed the viewport). The caller owns flushing
    /// so it can batch a whole render frame.
    ///
    /// `all_lines` is the complete content (not just the visible
    /// slice). `prev_viewport_top` is where the viewport was on
    /// the previous frame. `height` is the terminal height.
    /// `desired_cursor` is `(row, col)` in absolute line indices.
    pub fn render_scrolling(
        &mut self,
        w: &mut impl Write,
        all_lines: &[Vec<Cell>],
        prev_viewport_top: usize,
        height: usize,
        desired_cursor: (usize, usize),
    ) -> io::Result<()> {
        let normalized_all_lines = normalize_cell_lines(all_lines);
        let all_lines = normalized_all_lines.as_slice();
        let total = all_lines.len();
        let new_viewport_top = total.saturating_sub(height);

        let Some(changed_range) = self.scrolling_changed_range(all_lines, prev_viewport_top) else {
            let cursor_screen = desired_cursor.0.saturating_sub(new_viewport_top);
            self.move_to(w, cursor_screen, desired_cursor.1)?;
            return Ok(());
        };

        // Clamp first to the previous viewport — we can't render
        // above it (those rows aren't on the physical terminal).
        let render_start = changed_range.first_line.max(prev_viewport_top);
        let mut viewport_top = prev_viewport_top;
        self.scroll_to_render_start(w, render_start, &mut viewport_top, height)?;
        self.render_changed_scrolling_lines(
            w,
            all_lines,
            render_start,
            changed_range.last_line,
            &mut viewport_top,
            height,
        )?;

        // Clear any leftover lines below if content shrunk.
        let old_end = prev_viewport_top + self.lines.len();
        self.clear_shrunk_scrolling_lines(
            w,
            changed_range.last_line + 1,
            old_end,
            viewport_top,
            height,
        )?;

        // Position cursor.
        let cursor_screen = desired_cursor.0.saturating_sub(new_viewport_top);
        self.move_to(w, cursor_screen, desired_cursor.1)?;
        // Update tracked state to the new visible viewport.
        self.lines = all_lines[new_viewport_top..].to_vec();
        self.cursor_row = cursor_screen;
        self.cursor_col = desired_cursor.1;

        Ok(())
    }

    fn scrolling_changed_range(
        &self,
        all_lines: &[Vec<Cell>],
        prev_viewport_top: usize,
    ) -> Option<ChangedLineRange> {
        // Find first and last changed line across the part of the content that
        // is, or was, physically represented on the terminal. Lines above the
        // previous viewport are already in scrollback; treating them as changed
        // would force us to rewrite the top visible rows just before they drop
        // into scrollback.
        //
        // Keep missing lines distinct from present-but-empty lines: appending
        // an empty physical row still needs to scroll the viewport.
        let max_idx = all_lines.len().max(prev_viewport_top + self.lines.len());
        let mut changed_range: Option<ChangedLineRange> = None;
        for line_idx in prev_viewport_top..max_idx {
            let old = self
                .lines
                .get(line_idx - prev_viewport_top)
                .map(|line| line.as_slice());
            let new = all_lines.get(line_idx).map(|line| line.as_slice());
            if old != new {
                changed_range = Some(match changed_range {
                    Some(range) => ChangedLineRange {
                        first_line: range.first_line,
                        last_line: line_idx,
                    },
                    None => ChangedLineRange {
                        first_line: line_idx,
                        last_line: line_idx,
                    },
                });
            }
        }
        changed_range
    }

    fn scroll_to_render_start(
        &mut self,
        w: &mut impl Write,
        render_start: usize,
        viewport_top: &mut usize,
        height: usize,
    ) -> io::Result<()> {
        // Mutates both the tracked viewport and cursor row via natural terminal
        // scrolling before addressing `render_start` within the new viewport.
        let viewport_bottom = *viewport_top + height - 1;
        if render_start > viewport_bottom {
            let to_bottom = (height - 1).saturating_sub(self.cursor_row);
            self.move_down_rows(w, to_bottom)?;
            let scroll = render_start - viewport_bottom;
            self.move_down_rows(w, scroll)?;
            *viewport_top += scroll;
            self.cursor_row = height - 1;
        }
        let start_screen_row = render_start - *viewport_top;
        self.move_to(w, start_screen_row, 0)
    }

    fn render_changed_scrolling_lines(
        &mut self,
        w: &mut impl Write,
        all_lines: &[Vec<Cell>],
        render_start: usize,
        render_last_line: usize,
        viewport_top: &mut usize,
        height: usize,
    ) -> io::Result<()> {
        // `render_last_line` is inclusive. Missing rows are deliberate: they
        // represent old content that disappeared and should be cleared.
        for line_idx in render_start..=render_last_line {
            if render_start < line_idx {
                self.advance_scrolling_render_row(w, viewport_top, height)?;
            }
            w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            if let Some(line) = all_lines.get(line_idx) {
                emit_styled_cells(w, line)?;
            }
            self.cursor_col = all_lines
                .get(line_idx)
                .map(|line| cell_slice_cols(line))
                .unwrap_or(0);
        }
        Ok(())
    }

    fn clear_shrunk_scrolling_lines(
        &mut self,
        w: &mut impl Write,
        rendered_up_to: usize,
        old_end: usize,
        viewport_top: usize,
        height: usize,
    ) -> io::Result<()> {
        // Called immediately after rendering through `rendered_up_to - 1`; any
        // remaining old rows still inside the current viewport must be cleared.
        if old_end <= rendered_up_to {
            return Ok(());
        }
        for _ in rendered_up_to..old_end.min(viewport_top + height) {
            self.move_down_one(w)?;
            w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            if self.cursor_row + 1 < height {
                self.cursor_row += 1;
            }
        }
        Ok(())
    }

    fn advance_scrolling_render_row(
        &mut self,
        w: &mut impl Write,
        viewport_top: &mut usize,
        height: usize,
    ) -> io::Result<()> {
        self.move_down_one(w)?;
        let screen_row = self.cursor_row + 1;
        if screen_row >= height {
            // Moving down scrolled the terminal.
            *viewport_top += 1;
            self.cursor_row = height - 1;
        } else {
            self.cursor_row = screen_row;
        }
        Ok(())
    }

    fn move_down_rows(&mut self, w: &mut impl Write, rows: usize) -> io::Result<()> {
        for _ in 0..rows {
            self.move_down_one(w)?;
        }
        Ok(())
    }

    /// Resets the actual state to empty. Next `update()` treats everything as new.
    pub fn invalidate(&mut self) {
        self.lines.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Moves cursor to top of area and clears everything below.
    pub fn erase_all(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.cursor_row > 0 {
            w.queue(MoveUp(self.cursor_row as u16))?;
        }
        w.queue(MoveToColumn(0))?
            .queue(terminal::Clear(ClearType::FromCursorDown))?;
        self.cursor_row = 0;
        self.cursor_col = 0;
        Ok(())
    }

    /// Overwrites internal state. Call after a full render.
    pub fn reset_to(&mut self, lines: Vec<Vec<Cell>>, cursor_row: usize, cursor_col: usize) {
        self.lines = normalize_cell_lines(&lines);
        self.cursor_row = cursor_row;
        self.cursor_col = cursor_col;
    }

    // --- private helpers ---

    fn move_to(&mut self, w: &mut impl Write, row: usize, col: usize) -> io::Result<()> {
        if row < self.cursor_row {
            w.queue(MoveUp((self.cursor_row - row) as u16))?;
        } else if row > self.cursor_row {
            let down = row - self.cursor_row;
            for _ in 0..down {
                self.move_down_one(w)?;
            }
        }

        if col != self.cursor_col {
            w.queue(MoveToColumn(col as u16))?;
        }

        self.cursor_row = row;
        self.cursor_col = col;
        Ok(())
    }

    fn leave_pending_wrap_for_clear(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.width <= self.cursor_col {
            self.move_down_one(w)?;
            self.cursor_row += 1;
        }
        Ok(())
    }

    fn move_down_one(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.cursor_col != 0 {
            w.queue(MoveToColumn(0))?;
            self.cursor_col = 0;
        }
        w.queue(Print("\n"))?;
        Ok(())
    }
}
