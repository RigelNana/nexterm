//! Group management utilities for the session tree.

use uuid::Uuid;

use crate::SessionGroup;

impl SessionGroup {
    /// Create a new root-level group.
    pub fn new_root(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            parent_id: None,
            sort_order: 0,
        }
    }

    /// Create a child group under a parent.
    pub fn new_child(name: impl Into<String>, parent_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            parent_id: Some(parent_id),
            sort_order: 0,
        }
    }
}
