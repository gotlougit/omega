//! Agent runtime
//!
//! Provides:
//! - `AgentRuntime` - Spawns and manages agent tasks
//! - Channel-based communication between tasks

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::runtime::{AgentHandle, AgentInternals};
use crate::session::AgentSession;
use omega_core::core::{AgentContext, AgentState, FrameworkError, InputMessage, OutputChunk};

/// Runtime for spawning and managing agent tasks.
///
/// Each agent runs in its own tokio task and communicates via channels.
#[derive(Clone)]
pub struct AgentRuntime;

impl AgentRuntime {
    /// Create a new runtime
    pub fn new() -> Self {
        Self
    }

    /// Spawn a new agent task
    pub async fn spawn<F, Fut>(
        &self,
        session: AgentSession,
        agent_fn: F,
    ) -> Result<AgentHandle, FrameworkError>
    where
        F: FnOnce(AgentInternals) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), FrameworkError>> + Send + 'static,
    {
        let (input_tx, input_rx) = tokio::sync::mpsc::channel::<InputMessage>(256);
        let (output_tx, _output_rx) = tokio::sync::broadcast::channel::<OutputChunk>(256);
        let state = Arc::new(RwLock::new(AgentState::Idle));

        let context = AgentContext::new(
            session.session_id(),
            session.agent_type(),
            session.name(),
            session.description(),
        );

        let session_arc = Arc::new(RwLock::new(session));
        let session_id = session_arc.read().await.session_id().to_string();

        let internals = AgentInternals::new(
            Arc::clone(&session_arc),
            context,
            input_rx,
            output_tx.clone(),
            Arc::clone(&state),
        );

        let handle = AgentHandle::new(session_id, session_arc, input_tx, output_tx.clone(), state);

        tokio::spawn(async move {
            if let Err(e) = agent_fn(internals).await {
                tracing::error!("Agent task failed: {:?}", e);
            }
        });

        Ok(handle)
    }
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::new()
    }
}
