//! Public data types returned by a [`crate::DockerBackend`].

use std::collections::BTreeMap;

/// Runtime state of a container, derived from the `State` column emitted by
/// `docker ps --format '{{json .}}'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerStatus {
    Running,
    /// Exit code parsed from the human-readable `Status` (e.g. `Exited (0) 5 minutes ago`).
    Exited {
        code: Option<i32>,
    },
    Paused,
    Restarting,
    Created,
    Removing,
    Dead,
    /// State string Docker reported that we don't understand yet.
    Unknown(String),
}

impl ContainerStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Dead)
    }

    /// Short label suitable for a GUI pill / badge.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited { .. } => "exited",
            Self::Paused => "paused",
            Self::Restarting => "restarting",
            Self::Created => "created",
            Self::Removing => "removing",
            Self::Dead => "dead",
            Self::Unknown(_) => "unknown",
        }
    }
}

/// A single port mapping. Maps to one entry inside Docker's `Ports` column.
///
/// Examples of source strings:
/// * `"0.0.0.0:5432->5432/tcp"`          → host_ip=0.0.0.0, host_port=5432, container_port=5432
/// * `"[::]:5432->5432/tcp"`             → host_ip=::,      host_port=5432, container_port=5432
/// * `"5432/tcp"` (internal only)        → host_ip=None,    host_port=None, container_port=5432
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    pub host_ip: Option<String>,
    pub host_port: Option<u16>,
    pub container_port: u16,
    pub protocol: String,
}

/// Everything the container list panel needs for a single container row.
///
/// Populated by [`crate::parse_ps_lines`] from Docker's JSON-lines output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Full or short container ID, depending on whether `--no-trunc` was used
    /// when calling `docker ps`.
    pub id: String,
    /// Container names (Docker allows comma-separated aliases).
    pub names: Vec<String>,
    pub image: String,
    pub command: String,
    /// Raw `CreatedAt` string from Docker (e.g. `"2025-01-15 10:23:45 +0800 CST"`).
    /// Kept as `String` for now — parsing Go's time format is deferred until
    /// a caller actually needs a typed timestamp.
    pub created_at: String,
    pub status: ContainerStatus,
    /// Human-readable status (e.g. `"Up 3 hours"`, `"Exited (0) 2 days ago"`).
    pub status_raw: String,
    pub ports: Vec<PortMapping>,
    /// Size column from `docker ps -s` (empty string when not requested).
    pub size: String,
    pub labels: BTreeMap<String, String>,
}

impl ContainerInfo {
    /// Primary display name — first entry in [`Self::names`], or the ID.
    pub fn display_name(&self) -> &str {
        self.names.first().map(String::as_str).unwrap_or(&self.id)
    }

    /// Short 12-char ID for display (Docker's convention).
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }
}
