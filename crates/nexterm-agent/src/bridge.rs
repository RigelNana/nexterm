//! Agent Bridge: orchestrates the Agenium Agent lifecycle within NexTerm.
//!
//! The bridge owns an Agenium [`Agent`] and communicates with the NexTerm
//! main loop via channels. The GUI sends [`AgentBridgeCommand`]s; the bridge
//! responds with [`AgentBridgeEvent`]s (streaming text deltas, tool calls, etc.).

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use agent_context::engine::CompressionBudget;
use agent_core::agent::{Agent, AgentConfig};
use agent_core::event::AgentEvent;
use agent_core::message::DefaultMessageConverter;
use agent_permission::AllowAllPermissionGate;
use agent_permission::gate::PermissionContext;
use agent_provider::model::Model;
use agent_provider::traits::StreamOptions;
use agent_tool::definition::ToolExecutionMode;
use agent_vault::EnvVaultBackend;

use crate::tools::{self, ToolRequest};

// ---------------------------------------------------------------------------
// Events emitted to UI
// ---------------------------------------------------------------------------

/// Events emitted by the Agent bridge to the UI layer.
#[derive(Debug, Clone)]
pub enum AgentBridgeEvent {
    /// Agent produced a text response (streaming delta).
    TextDelta(String),
    /// Agent is invoking a tool.
    ToolCallStart {
        tool_name: String,
        args_summary: String,
    },
    /// Tool call completed.
    ToolCallEnd { tool_name: String, is_error: bool },
    /// Agent thinking text (for models that expose chain-of-thought).
    Thinking(String),
    /// Agent conversation completed.
    Done,
    /// Agent encountered an error.
    Error(String),
}

// ---------------------------------------------------------------------------
// Commands from UI
// ---------------------------------------------------------------------------

/// Commands sent to the Agent bridge from the UI layer.
#[derive(Debug)]
pub enum AgentBridgeCommand {
    /// User sends a natural language query.
    Query {
        message: String,
        /// Optional terminal context to include (e.g. recent output).
        terminal_context: Option<String>,
    },
    /// Cancel the current agent run.
    Cancel,
    /// Reset the conversation (clear history).
    Reset,
    /// Update the provider/model configuration.
    Configure {
        provider_type: String,
        base_url: String,
        api_key: String,
        model_id: String,
    },
}

// ---------------------------------------------------------------------------
// Bridge configuration
// ---------------------------------------------------------------------------

/// Configuration for creating the Agent bridge.
#[derive(Debug, Clone)]
pub struct AgentBridgeConfig {
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    pub system_prompt: String,
}

impl Default for AgentBridgeConfig {
    fn default() -> Self {
        Self {
            provider_type: "openai".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: String::new(),
            model_id: "deepseek-chat".into(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
        }
    }
}

const DEFAULT_SYSTEM_PROMPT: &str = r#"You are an AI assistant embedded in NexTerm, a GPU-accelerated terminal emulator with SSH support.

You help the user with:
- Running and debugging shell commands (local and remote via SSH)
- System administration and monitoring
- DevOps tasks (deployment, Docker, Kubernetes, etc.)
- Analyzing command output and logs
- Troubleshooting errors

You have access to tools that let you:
1. execute_command — Run a command in the user's terminal pane
2. read_terminal_output — Read recent terminal output
3. read_system_status — Check CPU, memory, disk, and process info

Be concise. Prefer actionable answers. When running commands, explain what you're doing briefly.
If a command might be destructive, warn the user first."#;

// ---------------------------------------------------------------------------
// Bridge handle (GUI side)
// ---------------------------------------------------------------------------

/// The Agent Bridge handle held by the GUI/main loop.
pub struct AgentBridge {
    pub command_tx: mpsc::Sender<AgentBridgeCommand>,
    pub event_rx: mpsc::Receiver<AgentBridgeEvent>,
    /// Channel for receiving tool requests from the agent worker.
    pub tool_request_rx: mpsc::Receiver<ToolRequest>,
}

impl AgentBridge {
    /// Create a new AgentBridge. Returns the bridge handle and a worker to be spawned.
    pub fn new(config: AgentBridgeConfig) -> (Self, AgentBridgeWorker) {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (evt_tx, evt_rx) = mpsc::channel(256);
        let (tool_req_tx, tool_req_rx) = mpsc::channel(32);

        let bridge = Self {
            command_tx: cmd_tx,
            event_rx: evt_rx,
            tool_request_rx: tool_req_rx,
        };
        let worker = AgentBridgeWorker {
            command_rx: cmd_rx,
            event_tx: evt_tx,
            tool_request_tx: tool_req_tx,
            config,
            cancel: CancellationToken::new(),
        };

        (bridge, worker)
    }
}

// ---------------------------------------------------------------------------
// Bridge worker (tokio task side)
// ---------------------------------------------------------------------------

/// Background worker that processes AgentBridge commands using Agenium.
pub struct AgentBridgeWorker {
    command_rx: mpsc::Receiver<AgentBridgeCommand>,
    event_tx: mpsc::Sender<AgentBridgeEvent>,
    tool_request_tx: mpsc::Sender<ToolRequest>,
    config: AgentBridgeConfig,
    cancel: CancellationToken,
}

impl AgentBridgeWorker {
    /// Run the worker loop (should be spawned as a tokio task).
    pub async fn run(mut self) -> Result<()> {
        let mut agent: Option<Agent> = None;

        while let Some(cmd) = self.command_rx.recv().await {
            match cmd {
                AgentBridgeCommand::Configure {
                    provider_type,
                    base_url,
                    api_key,
                    model_id,
                } => {
                    self.config.provider_type = provider_type;
                    self.config.base_url = base_url;
                    self.config.api_key = api_key;
                    self.config.model_id = model_id;
                    agent = None; // force re-creation
                    tracing::info!("agent reconfigured");
                }
                AgentBridgeCommand::Reset => {
                    if let Some(ref mut a) = agent {
                        a.reset();
                    }
                    tracing::info!("agent conversation reset");
                }
                AgentBridgeCommand::Cancel => {
                    self.cancel.cancel();
                    self.cancel = CancellationToken::new();
                    tracing::info!("agent run cancelled");
                }
                AgentBridgeCommand::Query {
                    message,
                    terminal_context,
                } => {
                    if self.config.api_key.is_empty() {
                        let _ = self.event_tx.send(AgentBridgeEvent::Error(
                            "No API key configured. Go to Settings → Agent to set your API key.".into()
                        )).await;
                        continue;
                    }

                    // Lazily create agent
                    if agent.is_none() {
                        match self.create_agent() {
                            Ok(a) => agent = Some(a),
                            Err(e) => {
                                let _ = self
                                    .event_tx
                                    .send(AgentBridgeEvent::Error(format!(
                                        "Failed to create agent: {e}"
                                    )))
                                    .await;
                                continue;
                            }
                        }
                    }

                    let a = agent.as_mut().unwrap();

                    // Build the full prompt with optional terminal context
                    let full_prompt = if let Some(ctx) = terminal_context {
                        format!("{message}\n\n<terminal_context>\n{ctx}\n</terminal_context>")
                    } else {
                        message
                    };

                    let event_tx = self.event_tx.clone();
                    let result = a
                        .query(&full_prompt, &self.cancel, move |event| {
                            let tx = event_tx.clone();
                            match &event {
                                AgentEvent::MessageUpdate {
                                    event: provider_event,
                                    ..
                                } => {
                                    use agent_provider::AssistantMessageEvent;
                                    match provider_event {
                                        AssistantMessageEvent::TextDelta { delta, .. } => {
                                            let _ = tx.try_send(AgentBridgeEvent::TextDelta(
                                                delta.clone(),
                                            ));
                                        }
                                        _ => {}
                                    }
                                }
                                AgentEvent::ToolExecutionStart {
                                    tool_name, args, ..
                                } => {
                                    let summary = serde_json::to_string(args)
                                        .unwrap_or_default()
                                        .chars()
                                        .take(120)
                                        .collect::<String>();
                                    let _ = tx.try_send(AgentBridgeEvent::ToolCallStart {
                                        tool_name: tool_name.clone(),
                                        args_summary: summary,
                                    });
                                }
                                AgentEvent::ToolExecutionEnd {
                                    tool_name,
                                    is_error,
                                    ..
                                } => {
                                    let _ = tx.try_send(AgentBridgeEvent::ToolCallEnd {
                                        tool_name: tool_name.clone(),
                                        is_error: *is_error,
                                    });
                                }
                                AgentEvent::Error { error } => {
                                    let _ = tx.try_send(AgentBridgeEvent::Error(error.clone()));
                                }
                                _ => {}
                            }
                        })
                        .await;

                    match result {
                        Ok(_) => {
                            let _ = self.event_tx.send(AgentBridgeEvent::Done).await;
                        }
                        Err(e) => {
                            let _ = self
                                .event_tx
                                .send(AgentBridgeEvent::Error(format!("{e}")))
                                .await;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn create_agent(&self) -> Result<Agent> {
        let provider: Arc<dyn agent_provider::traits::ProviderBackend> =
            match self.config.provider_type.as_str() {
                "anthropic" => Arc::new(
                    agent_provider::AnthropicProvider::new(&self.config.api_key)
                        .with_base_url(&self.config.base_url),
                ),
                _ => Arc::new(
                    agent_provider::OpenAIProvider::new(&self.config.api_key)
                        .with_base_url(&self.config.base_url),
                ),
            };

        let model = match self.config.provider_type.as_str() {
            "anthropic" => Model::anthropic(&self.config.model_id),
            _ => Model::openai(&self.config.model_id),
        };

        let nexterm_tools = tools::build_nexterm_tools(self.tool_request_tx.clone());

        let config = AgentConfig {
            model,
            provider,
            stream_options: StreamOptions::default(),
            tools: nexterm_tools,
            tool_execution_mode: ToolExecutionMode::Sequential,
            message_converter: Arc::new(DefaultMessageConverter),
            context_transform: None,
            permission_gate: Arc::new(AllowAllPermissionGate),
            permission_context: PermissionContext {
                session_id: "nexterm".into(),
                mode: agent_permission::gate::RuntimeMode::Cli,
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            },
            context_engine: None,
            compression_budget: CompressionBudget::default(),
            vault: Arc::new(EnvVaultBackend::new()),
            max_turns: Some(20),
            max_cost_usd: None,
            max_output_recovery_limit: 3,
            max_tool_concurrency: 1,
            before_tool_call: None,
            after_tool_call: None,
            system_prompt: self.config.system_prompt.clone(),
            prompt_builder: None,
        };

        Ok(Agent::new(config, 256))
    }
}
