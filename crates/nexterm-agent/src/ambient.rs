//! Ambient Agent: background listener that monitors terminal output for errors.

use tokio::sync::mpsc;

/// Ambient Agent that passively watches terminal output streams.
pub struct AmbientAgent {
    /// Channel to receive terminal output for monitoring.
    output_rx: mpsc::Receiver<AmbientInput>,
}

/// Input fed to the ambient agent.
#[derive(Debug, Clone)]
pub struct AmbientInput {
    pub pane_id: uuid::Uuid,
    /// Raw terminal output text.
    pub text: String,
    /// Exit code if a command just finished.
    pub exit_code: Option<i32>,
}

/// Suggestion produced by the ambient agent.
#[derive(Debug, Clone)]
pub struct AmbientSuggestion {
    pub pane_id: uuid::Uuid,
    pub message: String,
    /// Whether to auto-show or just badge the pane.
    pub auto_show: bool,
}

impl AmbientAgent {
    pub fn new(output_rx: mpsc::Receiver<AmbientInput>) -> Self {
        Self { output_rx }
    }

    /// Run the ambient monitoring loop.
    pub async fn run(mut self, suggestion_tx: mpsc::Sender<AmbientSuggestion>) {
        while let Some(input) = self.output_rx.recv().await {
            // Quick heuristic checks before invoking the LLM
            let should_analyze = input.exit_code.map(|c| c != 0).unwrap_or(false)
                || contains_error_pattern(&input.text);

            if should_analyze {
                tracing::debug!(pane_id = %input.pane_id, "ambient agent: error detected, queuing analysis");
                // TODO: invoke Agenium agent with error context for diagnosis
                let _ = suggestion_tx.send(AmbientSuggestion {
                    pane_id: input.pane_id,
                    message: format!("Error detected. Would you like AI to diagnose?"),
                    auto_show: true,
                }).await;
            }
        }
    }
}

/// Simple pattern matching for common error indicators.
fn contains_error_pattern(text: &str) -> bool {
    let patterns = [
        "error:", "Error:", "ERROR",
        "fatal:", "Fatal:", "FATAL",
        "panic:", "PANIC",
        "command not found",
        "Permission denied",
        "No such file or directory",
        "Connection refused",
        "Segmentation fault",
    ];
    patterns.iter().any(|p| text.contains(p))
}
