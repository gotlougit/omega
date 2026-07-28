//! Minimal TUI primitives adapted from tau crates.
//!
//! Contains the style types, diff-based screen renderer, and a
//! terminal prompt with block-based output zones.

pub mod emulator;
pub mod screen;
pub mod style;
pub mod term;

pub use screen::Screen;
pub use style::{
    display_width, emit_styled_cells, layout_block, layout_lines, truncate_to_width, Align,
    BlockId, Cell, Color, Span, Style, StyledBlock, StyledText,
};
pub use term::{Event, RawEvent, Term, TermHandle};
