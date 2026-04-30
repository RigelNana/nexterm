//! Agent Bridge: orchestrates the Agenium Agent lifecycle within NexTerm.

use anyhow::Result;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Events emitted by the Agent bridge to the UI layer.
#[derive(Debug, Clone)]
pub enum AgentBridgeEvent {
    /// Agent produced a text response (streaming delta).
    TextDelta { conversation_id: Uuid, delta: String },
    /// Agent wants to execute a command in a specific pane.
    ExecuteCommand { pane_id: Uuid, command: String },
    /// Agent requests user confirmation before a dangerous action.
    ConfirmAction { conversation_id: Uuid, description: String },
    /// Agent conversation completed.
    Done { conversation_id: Uuid },
    /// Agent encountered an error.
    Error { conversation_id: Uuid, error: String },
}

/// Commands sent to the Agent bridge from the UI layer.
#[derive(Debug, Clone)]
pub enum AgentBridgeCommand {
    /// User sends a natural language query.
    Query { pane_id: Uuid, message: String },
    /// User confirms a pending action.
    Confirm { conversation_id: Uuid, approved: bool },
    /// User wants to analyze a specific Block's output.
    AnalyzeBlock { pane_id: Uuid, block_output: String },
    /// Cancel an ongoing conversation.
    Cancel { conversation_id: Uuid },
}

/// The Agent Bridge controller.
pub struct AgentBridge {
    pub command_tx: mpsc::Sender<AgentBridgeCommand>,
    pub event_rx: mpsc::Receiver<AgentBridgeEvent>,
}

impl AgentBridge {
    /// Create a new AgentBridge. Returns the bridge handle and a task-spawning future.
    pub fn new() -> (Self, AgentBridgeWorker) {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (evt_tx, evt_rx) = mpsc::channel(64);

        let bridge = Self {
            command_tx: cmd_tx,
            event_rx: evt_rx,
        };
        let worker = AgentBridgeWorker {
            command_rx: cmd_rx,
            event_tx: evt_tx,
        };

        (bridge, worker)
    }
}

/// Background worker that processes AgentBridge commands using Agenium.
pub struct AgentBridgeWorker {
    command_rx: mpsc::Receiver<AgentBridgeCommand>,
    event_tx: mpsc::Sender<AgentBridgeEvent>,
}

impl AgentBridgeWorker {
    /// Run the worker loop (should be spawned as a tokio task).
    pub async fn run(mut self) -> Result<()> {
        while let Some(cmd) = self.command_rx.recv().await {
            match cmd {
                AgentBridgeCommand::Query { pane_id, message } => {
                    tracing::info!(pane_id = %pane_id, "agent query: {}", message);
                    // TODO: create Agenium Agent, register terminal tools, run ITO loop
                    // Forward streaming events via self.event_tx
                    let _ = self.event_tx.send(AgentBridgeEvent::TextDelta {
                        conversation_id: Uuid::new_v4(),
                        delta: format!("[Agent] Processing: {message}"),
                    }).await;
                }
                AgentBridgeCommand::Confirm { conversation_id, approved } => {
                    tracing::info!(conversation_id = %conversation_id, approved, "user confirmation");
                    // TODO: forward approval to the running agent
                }
                AgentBridgeCommand::AnalyzeBlock { pane_id, block_output } => {
                    tracing::info!(pane_id = %pane_id, "analyzing block output ({} bytes)", block_output.len());
                    // TODO: inject block output as context, run agent query
                }
                AgentBridgeCommand::Cancel { conversation_id } => {
                    tracing::info!(conversation_id = %conversation_id, "cancelling agent");
                    // TODO: signal cancellation token
                }
            }
        }
        Ok(())
    }
}
