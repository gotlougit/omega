pub mod core;
pub mod permissions;
pub mod runtime;
pub mod session;
pub mod tools;

// Optional components
pub mod cli;

pub mod llm;
pub mod logging;

// Useful helpers for agent implementations
pub mod helpers;

// Standardized agent implementation
pub mod agent;

// Hooks for intercepting agent behavior
pub mod hooks;

// Omega-sh client for delegating tool execution to a daemon
pub mod omega_client;

// omega-loop client for UI processes
pub mod omega_loop_client;

// Minimal TUI primitives (styled text/blocks adapted from tau)
pub mod tui;
