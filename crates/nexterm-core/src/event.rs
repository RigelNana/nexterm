//! Application-wide event types routed through the event bus.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Top-level events flowing through the NexTerm event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppEvent {
    /// Terminal output received from a PTY / SSH channel.
    TerminalOutput { pane_id: Uuid, data: Vec<u8> },
    /// User keyboard input destined for a specific pane.
    TerminalInput { pane_id: Uuid, data: Vec<u8> },
    /// Pane was created.
    PaneCreated { pane_id: Uuid },
    /// Pane was closed.
    PaneClosed { pane_id: Uuid },
    /// Tab was created.
    TabCreated { tab_id: Uuid },
    /// Tab was closed.
    TabClosed { tab_id: Uuid },
    /// Request to split the current pane.
    SplitPane { direction: SplitDirection },
    /// SSH connection state changed.
    SshStateChanged { session_id: Uuid, state: ConnectionState },
    /// AI agent event (forwarded from Agenium).
    AgentEvent { pane_id: Uuid, payload: String },
    /// Configuration reloaded.
    ConfigReloaded,
    /// Application shutdown requested.
    Shutdown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
    Reconnecting,
    Failed,
}
