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
pub use context_injection::InjectionChain;
pub use debugger::Debugger;
