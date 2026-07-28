//! Tool system for the omega agent framework
//!
//! This crate provides:
//! - `Tool` trait — interface for implementing tools
//! - `ToolRegistry` — registry for managing available tools
//! - Built-in tool implementations (Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion, Transfer)

mod registry;
mod tool;

/// Common/built-in tools
pub mod common;

pub use registry::ToolRegistry;
pub use tool::Tool;

// Re-export common tools for convenience
pub use common::{
    AskUserQuestionTool, BashTool, EditTool, GlobTool, GrepTool, ReadTool, TransferTool,
    WriteTool,
};
