//! Channel type definitions for agent communication
//!
//! Agents communicate via two channel types:
//! - **Input channel** (mpsc): Single-producer, single-consumer for sending commands to the agent
//! - **Output channel** (broadcast): Multi-consumer for streaming output to multiple subscribers

use tokio::sync::{broadcast, mpsc};

use omega_core::core::{InputMessage, OutputChunk};

// ============================================================================
// Channel Type Aliases
// ============================================================================

/// Sender half of the input channel (used by AgentHandle)
pub type InputSender = mpsc::Sender<InputMessage>;

/// Receiver half of the input channel (used by AgentInternals)
pub type InputReceiver = mpsc::Receiver<InputMessage>;

/// Sender half of the output broadcast channel (used by AgentInternals)
pub type OutputSender = broadcast::Sender<OutputChunk>;

/// Receiver half of the output broadcast channel (used by subscribers)
pub type OutputReceiver = broadcast::Receiver<OutputChunk>;
