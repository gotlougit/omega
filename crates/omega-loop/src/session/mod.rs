//! Session management for agents
//!
//! This module provides `AgentSession` for managing agent conversations,
//! history, metadata, and persistence.
//!
//! Each agent has its own session with a unique session_id. Sessions can
//! be linked via parent/child relationships for subagent tracking.

pub mod agent_session;
pub mod metadata;
pub mod storage;

pub use agent_session::AgentSession;
pub use storage::SessionStorage;
