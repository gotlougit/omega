//! Core types for the agent framework
//!
//! This module provides the fundamental types used throughout the framework:
//! - `AgentContext` - Hidden state passed to tools
//! - `AgentState` - Current state of an agent
//! - `OutputChunk` / `InputMessage` - Communication types
//! - `FrameworkError` - Error types
//! - `ToolRuntime` - Trait that tools use to interact with the agent runtime

pub mod context;
pub mod error;
pub mod output;
pub mod session;
pub mod state;
pub mod tool_runtime;

pub use context::{AgentContext, ResourceMap};
pub use error::{FrameworkError, FrameworkResult};
pub use output::{
    CacheTelemetry, InputMessage, OutputChunk, ToolInfo, ToolResult, ToolResultData,
};
pub use session::SessionInfo;
pub use state::AgentState;
pub use tool_runtime::ToolRuntime;
