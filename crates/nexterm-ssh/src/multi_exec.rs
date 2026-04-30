//! Multi-Exec: send a command to multiple SSH sessions simultaneously.

use uuid::Uuid;

/// Result of executing a command on a single target.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub session_id: Uuid,
    pub host: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Multi-Exec request.
#[derive(Debug, Clone)]
pub struct MultiExecRequest {
    pub command: String,
    pub targets: Vec<Uuid>,
    /// Delay between each target (0 = parallel).
    pub rolling_delay_ms: u64,
}
