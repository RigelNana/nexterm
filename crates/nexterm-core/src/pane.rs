//! Pane abstraction: each pane holds a terminal session (local PTY or SSH).

use uuid::Uuid;

/// The kind of backend driving a pane.
#[derive(Debug, Clone)]
pub enum PaneBackend {
    /// Local shell via PTY.
    Local,
    /// Remote shell via SSH.
    Ssh { session_id: Uuid },
}

/// A single terminal pane within a tab.
#[derive(Debug)]
pub struct Pane {
    pub id: Uuid,
    pub backend: PaneBackend,
    pub title: String,
    /// Current working directory (best-effort tracked).
    pub cwd: Option<String>,
    /// Scroll-back line count.
    pub scrollback: usize,
}

impl Pane {
    pub fn new_local() -> Self {
        Self {
            id: Uuid::new_v4(),
            backend: PaneBackend::Local,
            title: String::from("local"),
            cwd: None,
            scrollback: 0,
        }
    }

    pub fn new_ssh(session_id: Uuid, title: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            backend: PaneBackend::Ssh { session_id },
            title: title.into(),
            cwd: None,
            scrollback: 0,
        }
    }
}
