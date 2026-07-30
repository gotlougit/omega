//! AgentHandle - External interface for communicating with a running agent
//!
//! The `AgentHandle` is what external code (console, parent agent, tests) uses
//! to interact with a running agent. It provides methods to:
//! - Send input to the agent
//! - Subscribe to streaming output
//! - Request interrupt or shutdown

use omega_core::core::{FrameworkError, FrameworkResult, InputMessage};

use super::channels::{InputSender, OutputReceiver, OutputSender};

/// Handle for interacting with a running agent
///
/// This is the external interface for agent communication.
/// It can be cloned and shared across tasks.
#[derive(Clone)]
pub struct AgentHandle {
    /// Session ID of this agent
    session_id: String,

    /// Sender for input messages (to agent)
    input_tx: InputSender,

    /// Sender for output (for subscribing)
    output_tx: OutputSender,
}

impl AgentHandle {
    /// Create a new agent handle
    ///
    /// This is typically called by `AgentRuntime::spawn()`, not directly.
    pub fn new(
        session_id: impl Into<String>,
        input_tx: InputSender,
        output_tx: OutputSender,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            input_tx,
            output_tx,
        }
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
    use super::super::channels::InputReceiver;
    use super::*;
    use omega_core::core::OutputChunk;

    fn create_test_handle() -> (AgentHandle, InputReceiver) {
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(32);
        let (output_tx, _) = tokio::sync::broadcast::channel(256);
        let handle = AgentHandle::new("test-session", input_tx, output_tx);
        (handle, input_rx)
    }

    #[tokio::test]
    async fn test_send_input() {
        let (handle, mut rx) = create_test_handle();

        handle.send_input("Hello").await.unwrap();

        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, InputMessage::UserInput(s) if s == "Hello"));
    }

    #[tokio::test]
    async fn test_interrupt() {
        let (handle, mut rx) = create_test_handle();

        handle.interrupt().await.unwrap();

        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, InputMessage::Interrupt));
    }

    #[tokio::test]
    async fn test_shutdown() {
        let (handle, mut rx) = create_test_handle();

        handle.shutdown().await.unwrap();

        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, InputMessage::Shutdown));
    }

    #[tokio::test]
    async fn test_subscribe() {
        let (handle, _rx) = create_test_handle();

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
    async fn test_clone() {
        let (handle1, mut rx) = create_test_handle();
        let handle2 = handle1.clone();

        // Sending from either works
        handle1.send_input("From 1").await.unwrap();
        handle2.send_input("From 2").await.unwrap();

        let msg1 = rx.recv().await.unwrap();
        let msg2 = rx.recv().await.unwrap();

        assert!(matches!(msg1, InputMessage::UserInput(s) if s == "From 1"));
        assert!(matches!(msg2, InputMessage::UserInput(s) if s == "From 2"));
    }
}
