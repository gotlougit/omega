//! TUI (Terminal User Interface) module
//!
//! This module provides a rich terminal UI using Ratatui to replace the
//! simple CLI console. It includes:
//!
//! - `TuiApp` - The main application state machine and event loop
//! - `TuiRenderer` - Subscribes to an AgentHandle and renders output in the TUI
//! - Custom widgets for chat display, input, and status

pub mod app;
pub mod renderer;
pub mod widgets;

pub use app::TuiApp;
pub use renderer::TuiRenderer;
