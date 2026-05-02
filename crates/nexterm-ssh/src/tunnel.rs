//! SSH tunnel management: local/remote/dynamic port forwarding.

use serde::{Deserialize, Serialize};

/// SSH tunnel type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TunnelType {
    /// -L local_port:remote_host:remote_port
    LocalForward {
        local_port: u16,
        remote_host: String,
        remote_port: u16,
    },
    /// -R remote_port:local_host:local_port
    RemoteForward {
        remote_port: u16,
        local_host: String,
        local_port: u16,
    },
    /// -D local_port (SOCKS5 proxy)
    DynamicForward { local_port: u16 },
}

/// A configured tunnel template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelConfig {
    pub name: String,
    pub tunnel_type: TunnelType,
    pub auto_start: bool,
}
