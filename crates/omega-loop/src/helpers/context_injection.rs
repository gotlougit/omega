//! Context Injection System
//!
//! Allows programmers to modify messages before each LLM call without
//! writing the entire agent loop themselves.

use crate::runtime::AgentInternals;
use omega_llm::Message;

/// Trait for context injection implementations
pub trait ContextInjection: Send + Sync {
    /// Name of this injection (for logging/debugging)
    fn name(&self) -> &str;

    /// Inject context into messages before LLM call
    fn inject(&self, internals: &AgentInternals, messages: Vec<Message>) -> Vec<Message>;
}

/// Arc-wrapped injection for sharing across threads
pub type SharedInjection = std::sync::Arc<dyn ContextInjection>;

/// A chain of context injections that are applied in order
pub struct InjectionChain {
    injections: Vec<SharedInjection>,
}

impl InjectionChain {
    /// Create a new empty injection chain
    pub fn new() -> Self {
        Self {
            injections: Vec::new(),
        }
    }

    /// Apply all injections in order
    pub fn apply(&self, internals: &AgentInternals, mut messages: Vec<Message>) -> Vec<Message> {
        for injection in &self.injections {
            tracing::debug!("Applying context injection: {}", injection.name());
            messages = injection.inject(internals, messages);
        }
        messages
    }
}

impl Default for InjectionChain {
    fn default() -> Self {
        Self::new()
    }
}
