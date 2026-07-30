//! AgentHandle - External interface for communicating with a running agent
//!
//! The `AgentHandle` is what external code (console, parent agent, tests) uses
//! to interact with a running agent. It provides methods to:
//! - Send input to the agent
//! - Subscribe to streaming output
//! - Check agent state
//! - Request interrupt or shutdown

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::session::AgentSession;
use omega_core::core::{AgentState, FrameworkError, FrameworkResult, InputMessage};

use super::channels::{InputSender, OutputReceiver, OutputSender};

/// Handle for interacting with a running agent
///
/// This is the external interface for agent communication.
/// It can be cloned and shared across tasks.
#[derive(Clone)]
pub struct AgentHandle {
    /// Session ID of this agent
    session_id: String,

    /// Shared access to the agent's session
    #[allow(dead_code)]
    session: Arc<RwLock<AgentSession>>,

    /// Sender for input messages (to agent)
    input_tx: InputSender,

    /// Sender for output (for subscribing)
    output_tx: OutputSender,

    /// Current agent state
    #[allow(dead_code)]
    state: Arc<RwLock<AgentState>>,
}

impl AgentHandle {
    /// Create a new agent handle
    ///
    /// This is typically called by `AgentRuntime::spawn()`, not directly.
    pub fn new(
        session_id: impl Into<String>,
        session: Arc<RwLock<AgentSession>>,
        input_tx: InputSender,
        output_tx: OutputSender,
        state: Arc<RwLock<AgentState>>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            session,
            input_tx,
            output_tx,
            state,
        }
    }

    #[allow(dead_code)]
    /// Get the session ID
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    // =========================================================================
    // Input Methods
    // =========================================================================

    /// Send user input to the agent
    pub async fn send_input(&self, input: impl Into<String>) -> FrameworkResult<()> {
        self.send(InputMessage::UserInput(input.into())).await
    }

    /// Request graceful interrupt
    ///
    /// The agent should stop at the next safe point.
    pub async fn interrupt(&self) -> FrameworkResult<()> {
        self.send(InputMessage::Interrupt).await
    }

    /// Request shutdown
    ///
    /// The agent should terminate as soon as possible.
    pub async fn shutdown(&self) -> FrameworkResult<()> {
        self.send(InputMessage::Shutdown).await
    }

    /// Send any input message to the agent
    pub async fn send(&self, message: InputMessage) -> FrameworkResult<()> {
        self.input_tx
            .send(message)
            .await
            .map_err(|_| FrameworkError::ChannelClosed)
    }

    // =========================================================================
    // Output Methods
    // =========================================================================

    /// Subscribe to agent output
    ///
    /// Returns a receiver that will get all output chunks from this point forward.
    /// Multiple subscribers can exist simultaneously.
    pub fn subscribe(&self) -> OutputReceiver {
        self.output_tx.subscribe()
    }

    // =========================================================================
    // State Methods
    // =========================================================================
    // State Methods
    // =========================================================================

    /// Get the number of current subscribers
    #[allow(dead_code)]
    pub fn subscriber_count(&self) -> usize {
        self.output_tx.receiver_count()
    }

    /// Check if the agent is idle (waiting for input)
    #[allow(dead_code)]
    pub async fn is_idle(&self) -> bool {
        matches!(*self.state.read().await, AgentState::Idle)
    }

    /// Check if the agent is processing
    #[allow(dead_code)]
    pub async fn is_processing(&self) -> bool {
        matches!(*self.state.read().await, AgentState::Processing)
    }

    /// Check if the agent is done
    #[allow(dead_code)]
    pub async fn is_done(&self) -> bool {
        matches!(*self.state.read().await, AgentState::Done)
    }

    /// Check if the agent is still running (not done and not errored)
    #[allow(dead_code)]
    pub async fn is_running(&self) -> bool {
        let state = self.state.read().await;
        !matches!(*state, AgentState::Done | AgentState::Error { .. })
    }
}

impl std::fmt::Debug for AgentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHandle")
            .field("session_id", &self.session_id)
            .field("subscriber_count", &self.output_tx.receiver_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::channels::create_agent_channels;
    use crate::session::{AgentSession, SessionStorage};
    use omega_core::core::OutputChunk;
    use tempfile::TempDir;

    fn create_test_handle() -> (AgentHandle, super::super::channels::InputReceiver, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = SessionStorage::with_dir(temp_dir.path());
        let session = AgentSession::new_with_storage(
            "test-session",
            "test-agent",
            "Test Agent",
            "A test agent",
            "",
            storage,
        )
        .unwrap();
        let session = Arc::new(RwLock::new(session));

        let (input_tx, input_rx, output_tx) = create_agent_channels();
        let state = Arc::new(RwLock::new(AgentState::Idle));
        let handle = AgentHandle::new("test-session", session, input_tx, output_tx, state);
        (handle, input_rx, temp_dir)
    }

    #[tokio::test]
    async fn test_send_input() {
        let (handle, mut rx, _temp) = create_test_handle();

        handle.send_input("Hello").await.unwrap();

        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, InputMessage::UserInput(s) if s == "Hello"));
    }

    #[tokio::test]
    async fn test_interrupt() {
        let (handle, mut rx, _temp) = create_test_handle();

        handle.interrupt().await.unwrap();

        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, InputMessage::Interrupt));
    }

    #[tokio::test]
    async fn test_shutdown() {
        let (handle, mut rx, _temp) = create_test_handle();

        handle.shutdown().await.unwrap();

        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, InputMessage::Shutdown));
    }

    #[tokio::test]
    async fn test_subscribe() {
        let (handle, _rx, _temp) = create_test_handle();

        // Create subscribers
        let mut sub1 = handle.subscribe();
        let mut sub2 = handle.subscribe();

        // Simulate agent sending output (normally done by AgentInternals)
        // We access the internal output_tx for testing
        handle
            .output_tx
            .send(OutputChunk::TextDelta("Hi".into()))
            .unwrap();

        // Both subscribers receive
        let chunk1 = sub1.recv().await.unwrap();
        let chunk2 = sub2.recv().await.unwrap();

        assert!(matches!(chunk1, OutputChunk::TextDelta(s) if s == "Hi"));
        assert!(matches!(chunk2, OutputChunk::TextDelta(s) if s == "Hi"));
    }

    #[tokio::test]
    async fn test_state() {
        let temp_dir = TempDir::new().unwrap();
        let storage = SessionStorage::with_dir(temp_dir.path());
        let session =
            AgentSession::new_with_storage("test", "test-agent", "Test", "Test", "", storage)
                .unwrap();
        let session = Arc::new(RwLock::new(session));

        let (input_tx, _input_rx, output_tx) = create_agent_channels();
        let state = Arc::new(RwLock::new(AgentState::Idle));
        let handle = AgentHandle::new("test", session, input_tx, output_tx, state.clone());

        assert!(handle.is_idle().await);
        assert!(handle.is_running().await);

        // Change state
        *state.write().await = AgentState::Processing;
        assert!(handle.is_processing().await);

        *state.write().await = AgentState::Done;
        assert!(handle.is_done().await);
        assert!(!handle.is_running().await);
    }

    #[tokio::test]
    async fn test_session_id() {
        let (handle, _rx, _temp) = create_test_handle();
        assert_eq!(handle.session_id(), "test-session");
    }

    #[tokio::test]
    async fn test_clone() {
        let (handle1, mut rx, _temp) = create_test_handle();
        let handle2 = handle1.clone();

        // Both handles point to same session
        assert_eq!(handle1.session_id(), handle2.session_id());

        // Sending from either works
        handle1.send_input("From 1").await.unwrap();
        handle2.send_input("From 2").await.unwrap();

        let msg1 = rx.recv().await.unwrap();
        let msg2 = rx.recv().await.unwrap();

        assert!(matches!(msg1, InputMessage::UserInput(s) if s == "From 1"));
        assert!(matches!(msg2, InputMessage::UserInput(s) if s == "From 2"));
    }
}
