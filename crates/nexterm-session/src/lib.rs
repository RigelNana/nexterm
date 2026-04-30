//! # nexterm-session
//!
//! Session manager: tree-structured groups of SSH profiles, backed by SQLite.

pub mod store;
pub mod group;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A folder/group node in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    pub id: Uuid,
    pub name: String,
    pub parent_id: Option<Uuid>,
    /// Sort order within the parent.
    pub sort_order: i32,
}

/// Represents either a group or a session leaf in the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionTreeNode {
    Group {
        group: SessionGroup,
        children: Vec<SessionTreeNode>,
    },
    Session(nexterm_ssh::SshProfile),
}
