//! Terminal-specific tools registered with the Agenium agent.
//!
//! These tools allow the AI to:
//! - Execute commands in a terminal pane (local or SSH)
//! - Read terminal block output
//! - Read system status

use std::sync::Arc;

use agent_tool::definition::{ToolDefinition, ToolExecutionContext, ToolExecutionMode};
use agent_tool::error::ToolError;
use agent_tool::result::ToolResult;
use tokio::sync::{mpsc, oneshot};

/// Tool names exposed to the AI agent.
pub mod tool_names {
    pub const EXECUTE_COMMAND: &str = "execute_command";
    pub const READ_TERMINAL_OUTPUT: &str = "read_terminal_output";
    pub const READ_SYSTEM_STATUS: &str = "read_system_status";
}

// ---------------------------------------------------------------------------
// Shared request/response types for communicating with the main event loop
// ---------------------------------------------------------------------------

/// A request from an Agent tool to the NexTerm main loop.
#[derive(Debug)]
pub enum ToolRequest {
    /// Execute a command in the focused (or specified) pane.
    ExecuteCommand {
        command: String,
        pane_id: Option<usize>,
        /// Channel to send back the output once complete.
        reply: oneshot::Sender<ToolResponse>,
    },
    /// Read the last N blocks of terminal output from a pane.
    ReadTerminalOutput {
        pane_id: Option<usize>,
        last_n_blocks: usize,
        reply: oneshot::Sender<ToolResponse>,
    },
    /// Read the current system status snapshot.
    ReadSystemStatus {
        reply: oneshot::Sender<ToolResponse>,
    },
}

/// Response from the main loop back to an Agent tool.
#[derive(Debug, Clone)]
pub enum ToolResponse {
    /// Successful text output.
    Output(String),
    /// An error occurred.
    Error(String),
}

// ---------------------------------------------------------------------------
// ExecuteCommandTool
// ---------------------------------------------------------------------------

/// Executes a shell command in the user's terminal pane.
///
/// The command is written to the focused pane's PTY. The tool waits for
/// the command block to complete (or times out) and returns the output.
pub struct ExecuteCommandTool {
    pub request_tx: mpsc::Sender<ToolRequest>,
}

#[async_trait::async_trait]
impl ToolDefinition for ExecuteCommandTool {
    fn name(&self) -> &str {
        tool_names::EXECUTE_COMMAND
    }

    fn description(&self) -> &str {
        "Execute a command in the terminal"
    }

    fn prompt(&self) -> String {
        r#"Execute a shell command in the user's terminal (local shell or SSH session).
The command runs in the actual terminal pane visible to the user.
Use this to run diagnostics, check system state, deploy, etc.

Parameters:
- command (required): The shell command to execute.
- pane_id (optional): Target pane index. If omitted, uses the focused pane.

The tool returns the command's stdout/stderr output and exit code."#
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "pane_id": {
                    "type": "integer",
                    "description": "Target pane index (optional, defaults to focused pane)"
                }
            },
            "required": ["command"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        _ctx: ToolExecutionContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput {
                tool: tool_names::EXECUTE_COMMAND.into(),
                message: "missing 'command' parameter".into(),
            })?
            .to_string();

        let pane_id = args.get("pane_id").and_then(|v| v.as_u64()).map(|v| v as usize);

        // Step 1: Send the command to the terminal
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(ToolRequest::ExecuteCommand {
                command: command.clone(),
                pane_id,
                reply: reply_tx,
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to send request: {e}")))?;

        // Wait for acknowledgment that the command was written
        match reply_rx.await {
            Ok(ToolResponse::Error(err)) => return Ok(ToolResult::error(err)),
            Err(_) => return Ok(ToolResult::error("request channel closed")),
            Ok(_) => {} // success, command was sent
        }

        // Step 2: Wait for the command to finish executing
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

        // Step 3: Read back the terminal output
        let (reply_tx2, reply_rx2) = oneshot::channel();
        self.request_tx
            .send(ToolRequest::ReadTerminalOutput {
                pane_id,
                last_n_blocks: 1,
                reply: reply_tx2,
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to read output: {e}")))?;

        match reply_rx2.await {
            Ok(ToolResponse::Output(output)) => {
                Ok(ToolResult::text(format!("$ {command}\n\n{output}")))
            }
            Ok(ToolResponse::Error(err)) => Ok(ToolResult::error(err)),
            Err(_) => Ok(ToolResult::error("request channel closed")),
        }
    }
}

// ---------------------------------------------------------------------------
// ReadTerminalOutputTool
// ---------------------------------------------------------------------------

/// Reads recent terminal output from a pane (the last N command blocks).
pub struct ReadTerminalOutputTool {
    pub request_tx: mpsc::Sender<ToolRequest>,
}

#[async_trait::async_trait]
impl ToolDefinition for ReadTerminalOutputTool {
    fn name(&self) -> &str {
        tool_names::READ_TERMINAL_OUTPUT
    }

    fn description(&self) -> &str {
        "Read recent terminal output"
    }

    fn prompt(&self) -> String {
        r#"Read the recent terminal output from a pane.
Returns the visible content of the terminal buffer, including recent command outputs.

Parameters:
- pane_id (optional): Target pane index. If omitted, uses the focused pane.
- last_n_blocks (optional): Number of recent command blocks to return (default: 3)."#
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pane_id": {
                    "type": "integer",
                    "description": "Target pane index (optional)"
                },
                "last_n_blocks": {
                    "type": "integer",
                    "description": "Number of recent command blocks to read (default: 3)"
                }
            }
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        _ctx: ToolExecutionContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let pane_id = args.get("pane_id").and_then(|v| v.as_u64()).map(|v| v as usize);
        let last_n = args
            .get("last_n_blocks")
            .and_then(|v| v.as_u64())
            .unwrap_or(3) as usize;

        let (reply_tx, reply_rx) = oneshot::channel();

        self.request_tx
            .send(ToolRequest::ReadTerminalOutput {
                pane_id,
                last_n_blocks: last_n,
                reply: reply_tx,
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to send request: {e}")))?;

        match reply_rx.await {
            Ok(ToolResponse::Output(output)) => Ok(ToolResult::text(output)),
            Ok(ToolResponse::Error(err)) => Ok(ToolResult::error(err)),
            Err(_) => Ok(ToolResult::error("request channel closed")),
        }
    }
}

// ---------------------------------------------------------------------------
// ReadSystemStatusTool
// ---------------------------------------------------------------------------

/// Reads the current system status (CPU, memory, disk, top processes).
pub struct ReadSystemStatusTool {
    pub request_tx: mpsc::Sender<ToolRequest>,
}

#[async_trait::async_trait]
impl ToolDefinition for ReadSystemStatusTool {
    fn name(&self) -> &str {
        tool_names::READ_SYSTEM_STATUS
    }

    fn description(&self) -> &str {
        "Read system status"
    }

    fn prompt(&self) -> String {
        r#"Read the current system status of the connected host.
Returns CPU usage, memory usage, disk usage, network stats, and top processes.
This information comes from the system status probe and does not require running any commands."#
            .to_string()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _call_id: &str,
        _args: serde_json::Value,
        _ctx: ToolExecutionContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.request_tx
            .send(ToolRequest::ReadSystemStatus { reply: reply_tx })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to send request: {e}")))?;

        match reply_rx.await {
            Ok(ToolResponse::Output(output)) => Ok(ToolResult::text(output)),
            Ok(ToolResponse::Error(err)) => Ok(ToolResult::error(err)),
            Err(_) => Ok(ToolResult::error("request channel closed")),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: build the NexTerm tool set
// ---------------------------------------------------------------------------

/// Build the full set of NexTerm terminal tools.
pub fn build_nexterm_tools(
    request_tx: mpsc::Sender<ToolRequest>,
) -> Vec<Arc<dyn ToolDefinition>> {
    vec![
        Arc::new(ExecuteCommandTool {
            request_tx: request_tx.clone(),
        }),
        Arc::new(ReadTerminalOutputTool {
            request_tx: request_tx.clone(),
        }),
        Arc::new(ReadSystemStatusTool {
            request_tx,
        }),
    ]
}
