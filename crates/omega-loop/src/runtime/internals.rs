//! Agent internals — internal state passed to agent functions
//!
//! Provides `AgentInternals` with access to:
//! - Session read/write
//! - Input/output channels
//! - Context (current tool, state, resources)

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_trait::async_trait;
use tokio::sync::RwLock;

use omega_core::core::{
    output::UserQuestion, AgentContext, AgentState, FrameworkError, FrameworkResult, InputMessage,
    OutputChunk,
};
use crate::runtime::channels::{InputReceiver, OutputSender};
use crate::session::AgentSession;
use omega_core::core::{ToolResult, ToolRuntime};

/// Internal state passed to agent functions.
///
/// Provides access to session, communication channels, and context.
pub struct AgentInternals {
    /// The session (conversation history, metadata)
    pub session: Arc<RwLock<AgentSession>>,

    /// Agent context (current tool, metadata, resources)
    pub context: AgentContext,

    /// Agent state
    pub state: Arc<RwLock<AgentState>>,

    /// Interrupted flag
    pub is_interrupted: AtomicBool,

    /// Input channel receiver
    input_rx: InputReceiver,

    /// Output channel sender
    output_tx: OutputSender,
}

impl AgentInternals {
    /// Create a new AgentInternals
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: Arc<RwLock<AgentSession>>,
        context: AgentContext,
        input_rx: InputReceiver,
        output_tx: OutputSender,
        state: Arc<RwLock<AgentState>>,
    ) -> Self {
        Self {
            session,
            context,
            input_rx,
            output_tx,
            state,
            is_interrupted: AtomicBool::new(false),
        }
    }

    // ------------------------------------------------------------------
    // Input methods
    // ------------------------------------------------------------------

    /// Receive the next input message (mpsc, returns None when channel is closed)
    pub async fn receive(&mut self) -> Option<InputMessage> {
        self.input_rx.recv().await
    }



    // ------------------------------------------------------------------
    // Output methods
    // ------------------------------------------------------------------

    /// Send an output chunk to subscribers
    pub fn send(&self, chunk: OutputChunk) {
        let _ = self.output_tx.send(chunk);
    }

    /// Send a text delta
    pub fn send_text_delta(&self, text: &str) {
        self.send(OutputChunk::TextDelta(text.to_string()));
    }

    /// Send text complete
    pub fn send_text_complete(&self, text: &str) {
        self.send(OutputChunk::TextComplete(text.to_string()));
    }

    /// Send a thinking delta
    pub fn send_thinking_delta(&self, text: &str) {
        self.send(OutputChunk::ThinkingDelta(text.to_string()));
    }

    /// Send thinking complete
    pub fn send_thinking_complete(&self, text: &str) {
        self.send(OutputChunk::ThinkingComplete(text.to_string()));
    }

    /// Send a text block (delta + complete in one call)
    pub fn send_text(&self, text: &str) {
        self.send_text_delta(text);
    }

    /// Send a thinking block (delta + complete in one call)
    pub fn send_thinking(&self, text: &str) {
        self.send_thinking_delta(text);
    }

    /// Notify that a tool has started
    pub fn send_tool_start(&self, id: &str, name: &str, input: serde_json::Value) {
        self.send(OutputChunk::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
            input,
        });
    }

    /// Notify that a tool has ended
    pub fn send_tool_end(&self, id: &str, result: ToolResult) {
        self.send(OutputChunk::ToolEnd {
            id: id.to_string(),
            result,
        });
    }

    /// Send a status message
    pub fn send_status(&self, text: impl Into<String>) {
        self.send(OutputChunk::Status(text.into()));
    }

    /// Send an error message
    pub fn send_error(&self, text: impl Into<String>) {
        self.send(OutputChunk::Error(text.into()));
    }

    /// Send Done signal
    pub fn send_done(&self) {
        self.send(OutputChunk::Done);
    }

    // ------------------------------------------------------------------
    // Ask User (question/response flow)
    // ------------------------------------------------------------------

    /// Ask the user a question and wait for their response
    pub async fn ask_user_question(
        &mut self,
        request_id: impl Into<String>,
        questions: Vec<UserQuestion>,
    ) -> FrameworkResult<HashMap<String, String>> {
        let request_id = request_id.into();

        self.send(OutputChunk::AskUserQuestion {
            request_id: request_id.clone(),
            questions,
        });

        // Wait for response
        loop {
            match self.receive().await {
                Some(InputMessage::UserQuestionResponse {
                    request_id: resp_id,
                    answers,
                }) if resp_id == request_id => {
                    return Ok(answers);
                }
                Some(InputMessage::Interrupt) => {
                    return Err(FrameworkError::Interrupted);
                }
                Some(InputMessage::Shutdown) => {
                    return Err(FrameworkError::Shutdown);
                }
                Some(_) => continue,
                None => {
                    return Err(FrameworkError::ChannelClosed);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // State methods
    // ------------------------------------------------------------------

    /// Set the agent state and notify subscribers
    pub async fn set_state(&self, new_state: AgentState) {
        let mut state = self.state.write().await;
        *state = new_state.clone();
        let _ = self.output_tx.send(OutputChunk::StateChange(new_state));
    }

    /// Set state to Processing
    pub async fn set_processing(&self) {
        self.set_state(AgentState::Processing).await;
    }

    /// Set state to Idle
    pub async fn set_idle(&self) {
        self.set_state(AgentState::Idle).await;
    }

    /// Set state to Done
    pub async fn set_done(&self) {
        self.set_state(AgentState::Done).await;
    }

    /// Set state to ExecutingTool
    pub async fn set_executing_tool(&self, tool_name: &str, tool_id: &str) {
        self.set_state(AgentState::executing_tool(tool_name, tool_id))
            .await;
    }

    /// Prepare for the next turn
    pub fn next_turn(&mut self) {
        self.context.clear_tool_use_id();
    }

    // ------------------------------------------------------------------
    // Interruption
    // ------------------------------------------------------------------

    /// Check if the agent has been interrupted
    pub fn is_interrupted(&self) -> bool {
        self.is_interrupted.load(Ordering::SeqCst)
    }

    /// Check if the session is interactive (has user attached)
    pub fn is_interactive(&self) -> bool {
        true
    }
}

#[async_trait]
impl ToolRuntime for AgentInternals {
    fn send_output(&self, chunk: OutputChunk) {
        self.send(chunk);
    }

    async fn ask_user_question(
        &mut self,
        request_id: &str,
        questions: Vec<UserQuestion>,
    ) -> FrameworkResult<HashMap<String, String>> {
        self.ask_user_question(request_id, questions).await
    }

    fn is_interrupted(&self) -> bool {
        self.is_interrupted()
    }

    fn is_interactive(&self) -> bool {
        self.is_interactive()
    }
}
