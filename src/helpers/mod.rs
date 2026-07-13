//! Useful helpers for agent implementations
//!
//! This module provides reusable components that agents can opt-in to:
//! - `ContextInjection` - Modify messages before each LLM call
//! - `Debugger` - Log API calls and tool executions for debugging
//! - `Attachments` - Process file attachments in user messages

mod attachments;
mod context_injection;
mod debugger;

pub use attachments::process_attachments;
pub use context_injection::{
    append_to_last_message, inject_system_reminder, prepend_to_first_user_message, BoxedInjection,
    ContextInjection, FnInjection, InjectionChain, SharedInjection,
};
pub use debugger::{
    ApiRequestEvent, ApiResponseEvent, Debugger, EventType, ToolCallEvent, ToolResultEvent,
};
