//! Tab management: each tab contains one or more panes in a split layout.

use uuid::Uuid;

use crate::pane::Pane;

/// Layout tree node for split panes within a tab.
#[derive(Debug)]
pub enum LayoutNode {
    Leaf(Pane),
    Split {
        direction: SplitDir,
        /// Ratio of first child (0.0 – 1.0).
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// A single tab containing a layout tree of panes.
#[derive(Debug)]
pub struct Tab {
    pub id: Uuid,
    pub title: String,
    pub root: LayoutNode,
}

impl Tab {
    /// Create a tab with a single local pane.
    pub fn new_local() -> Self {
        Self {
            id: Uuid::new_v4(),
            title: String::from("Local"),
            root: LayoutNode::Leaf(Pane::new_local()),
        }
    }

    /// Create a tab with a single SSH pane.
    pub fn new_ssh(session_id: Uuid, title: impl Into<String>) -> Self {
        let title = title.into();
        Self {
            id: Uuid::new_v4(),
            title: title.clone(),
            root: LayoutNode::Leaf(Pane::new_ssh(session_id, title)),
        }
    }
}
