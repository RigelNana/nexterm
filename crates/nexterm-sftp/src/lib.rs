//! # nexterm-sftp
//!
//! SFTP engine: file browsing, upload/download, drag-drop, and edit-in-place.

pub mod browser;
pub mod transfer;

use serde::{Deserialize, Serialize};

/// Metadata for a remote file entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub permissions: u32,
    pub modified: u64,
    /// File type label (dir, file, link, etc.)
    pub file_type: String,
    /// Owner user name (or UID string).
    pub owner: String,
    /// Group name (or GID string).
    pub group: String,
}

/// Transfer direction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum TransferDirection {
    Upload,
    Download,
}

/// State of a file transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransferState {
    Queued,
    InProgress { bytes_transferred: u64, total_bytes: u64 },
    Paused { bytes_transferred: u64, total_bytes: u64 },
    Completed,
    Failed(String),
}

/// A tracked file transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferJob {
    pub id: uuid::Uuid,
    pub direction: TransferDirection,
    pub local_path: String,
    pub remote_path: String,
    pub state: TransferState,
}
