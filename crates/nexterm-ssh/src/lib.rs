//! # nexterm-ssh
//!
//! SSH connection lifecycle, tunneling, ProxyJump, Multi-Exec, and keep-alive.

pub mod config_parser;
pub mod connection;
pub mod multi_exec;
pub mod tunnel;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Authentication method for an SSH connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthMethod {
    /// Password authentication.
    Password(String),
    /// Public key file path (with optional passphrase).
    PublicKey {
        key_path: String,
        passphrase: Option<String>,
    },
    /// Use the system SSH agent.
    Agent,
    /// Interactive keyboard authentication.
    KeyboardInteractive,
}

/// SSH connection profile (stored in session manager).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshProfile {
    pub id: Uuid,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    /// Optional jump host chain.
    pub proxy_jump: Vec<String>,
    /// Custom environment variables to set on the remote.
    pub env: Vec<(String, String)>,
    /// Keep-alive interval in seconds (0 = disabled).
    pub keepalive_interval: u32,
    /// Tags for grouping/filtering.
    pub tags: Vec<String>,
    /// Group path (e.g., "Production/Web").
    pub group: Option<String>,
}

impl Default for SshProfile {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            host: String::new(),
            port: 22,
            username: String::from("root"),
            auth: AuthMethod::Agent,
            proxy_jump: Vec::new(),
            env: Vec::new(),
            keepalive_interval: 30,
            tags: Vec::new(),
            group: None,
        }
    }
}
