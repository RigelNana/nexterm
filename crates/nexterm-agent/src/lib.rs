//! # nexterm-agent
//!
//! Bridge layer between the Agenium AI engine and NexTerm's terminal/SSH subsystems.
//!
//! Responsibilities:
//! - Register terminal-specific tools (execute command, read file, SSH ops)
//! - Inject Block context into Agent conversations
//! - Route Agent tool-call results to the correct pane/session
//! - Manage Ambient Agent (background error detection)

pub mod bridge;
pub mod tools;
pub mod ambient;
