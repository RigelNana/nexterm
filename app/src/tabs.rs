//! Tab and layout management: tabs contain a layout tree of split panes.

use crate::pane::{Pane, Rect};
use nexterm_ssh::SshProfile;
use std::collections::HashMap;
use tokio::runtime::Runtime;
use tracing::info;

/// Split direction for pane layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    Horizontal, // left | right
    Vertical,   // top / bottom
}

/// Layout tree node.
#[derive(Debug, Clone)]
pub enum LayoutNode {
    Leaf(usize), // pane_id
    Split {
        direction: SplitDir,
        ratio: f32, // 0.0..1.0 position of divider
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    /// Collect all pane IDs in this layout.
    pub fn pane_ids(&self) -> Vec<usize> {
        match self {
            LayoutNode::Leaf(id) => vec![*id],
            LayoutNode::Split { first, second, .. } => {
                let mut ids = first.pane_ids();
                ids.extend(second.pane_ids());
                ids
            }
        }
    }

    /// Compute viewport rects for all panes given the available rect.
    pub fn compute_viewports(&self, rect: Rect, out: &mut Vec<(usize, Rect)>) {
        match self {
            LayoutNode::Leaf(id) => {
                out.push((*id, rect));
            }
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                // Snap split positions to integer pixels so all pane offsets are
                // integer-aligned. Fractional offsets cause atlas-sampling blur
                // and 1px top/bottom clipping.
                let (r1, r2) = match direction {
                    SplitDir::Horizontal => {
                        let split_x = (rect.x + rect.w * ratio).round();
                        let left = Rect {
                            x: rect.x,
                            y: rect.y,
                            w: (split_x - rect.x - 1.0).max(0.0), // 1px divider gap
                            h: rect.h,
                        };
                        let right = Rect {
                            x: split_x + 1.0,
                            y: rect.y,
                            w: (rect.x + rect.w - split_x - 1.0).max(0.0),
                            h: rect.h,
                        };
                        (left, right)
                    }
                    SplitDir::Vertical => {
                        let split_y = (rect.y + rect.h * ratio).round();
                        let top = Rect {
                            x: rect.x,
                            y: rect.y,
                            w: rect.w,
                            h: (split_y - rect.y - 1.0).max(0.0),
                        };
                        let bottom = Rect {
                            x: rect.x,
                            y: split_y + 1.0,
                            w: rect.w,
                            h: (rect.y + rect.h - split_y - 1.0).max(0.0),
                        };
                        (top, bottom)
                    }
                };
                first.compute_viewports(r1, out);
                second.compute_viewports(r2, out);
            }
        }
    }

    /// Find the node that contains a given pane_id and replace it with a split.
    /// Returns true if the split was performed.
    pub fn split_pane(
        &mut self,
        target_id: usize,
        new_pane_id: usize,
        direction: SplitDir,
    ) -> bool {
        match self {
            LayoutNode::Leaf(id) if *id == target_id => {
                let old = LayoutNode::Leaf(target_id);
                let new = LayoutNode::Leaf(new_pane_id);
                *self = LayoutNode::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(old),
                    second: Box::new(new),
                };
                true
            }
            LayoutNode::Split { first, second, .. } => {
                first.split_pane(target_id, new_pane_id, direction)
                    || second.split_pane(target_id, new_pane_id, direction)
            }
            _ => false,
        }
    }

    /// Remove a pane from the layout. Returns the replacement node (the sibling) if found.
    pub fn remove_pane(&mut self, target_id: usize) -> Option<LayoutNode> {
        match self {
            LayoutNode::Leaf(_) => None,
            LayoutNode::Split { first, second, .. } => {
                // Check if first child is the target leaf
                if let LayoutNode::Leaf(id) = first.as_ref() {
                    if *id == target_id {
                        return Some(*second.clone());
                    }
                }
                // Check if second child is the target leaf
                if let LayoutNode::Leaf(id) = second.as_ref() {
                    if *id == target_id {
                        return Some(*first.clone());
                    }
                }
                // Recurse into children
                if let Some(replacement) = first.remove_pane(target_id) {
                    *first = Box::new(replacement);
                    return None; // handled in-place
                }
                if let Some(replacement) = second.remove_pane(target_id) {
                    *second = Box::new(replacement);
                    return None;
                }
                None
            }
        }
    }
}

/// A single tab containing a layout tree.
pub struct Tab {
    pub title: String,
    pub layout: LayoutNode,
    pub focused_pane: usize,
}

/// Manages all tabs and panes.
pub struct TabManager {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub panes: HashMap<usize, Pane>,
    next_pane_id: usize,
}

impl TabManager {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            panes: HashMap::new(),
            next_pane_id: 0,
        }
    }

    fn alloc_pane_id(&mut self) -> usize {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        id
    }

    /// Create a new tab with a single pane. Returns the tab index.
    pub fn new_tab(&mut self, viewport: Rect, cell_w: f32, cell_h: f32, shell: Option<&str>) -> anyhow::Result<usize> {
        let pane_id = self.alloc_pane_id();
        let pane = Pane::new(pane_id, viewport, cell_w, cell_h, shell)?;
        self.panes.insert(pane_id, pane);

        let tab = Tab {
            title: format!("Tab {}", self.tabs.len() + 1),
            layout: LayoutNode::Leaf(pane_id),
            focused_pane: pane_id,
        };
        self.tabs.push(tab);
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        info!(tab_idx = idx, pane_id, "new tab created");
        Ok(idx)
    }

    /// Create a new tab with an SSH pane. Returns the tab index.
    pub fn new_ssh_tab(
        &mut self,
        viewport: Rect,
        cell_w: f32,
        cell_h: f32,
        profile: SshProfile,
        rt: &Runtime,
    ) -> usize {
        let pane_id = self.alloc_pane_id();
        let host = profile.host.clone();
        let pane = Pane::new_ssh(pane_id, viewport, cell_w, cell_h, profile, rt);
        self.panes.insert(pane_id, pane);

        let tab = Tab {
            title: format!("SSH: {}", host),
            layout: LayoutNode::Leaf(pane_id),
            focused_pane: pane_id,
        };
        self.tabs.push(tab);
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        info!(tab_idx = idx, pane_id, host = %host, "SSH tab created");
        idx
    }

    /// Close the active tab. Returns true if any tabs remain.
    pub fn close_active_tab(&mut self) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        let tab = self.tabs.remove(self.active_tab);
        // Remove all panes belonging to this tab
        for pane_id in tab.layout.pane_ids() {
            self.panes.remove(&pane_id);
        }
        if self.active_tab >= self.tabs.len() && !self.tabs.is_empty() {
            self.active_tab = self.tabs.len() - 1;
        }
        !self.tabs.is_empty()
    }

    /// Switch to next tab.
    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    /// Switch to previous tab.
    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = if self.active_tab == 0 {
                self.tabs.len() - 1
            } else {
                self.active_tab - 1
            };
        }
    }

    /// Split the focused pane in the active tab.
    pub fn split_focused(
        &mut self,
        direction: SplitDir,
        cell_w: f32,
        cell_h: f32,
        shell: Option<&str>,
    ) -> anyhow::Result<()> {
        let tab = &self.tabs[self.active_tab];
        let focused = tab.focused_pane;

        // The new pane gets a temporary viewport; will be recomputed on next layout pass.
        let viewport = self.panes[&focused].viewport;
        let new_id = self.alloc_pane_id();
        let half_rect = match direction {
            SplitDir::Horizontal => Rect {
                x: viewport.x + viewport.w * 0.5,
                y: viewport.y,
                w: viewport.w * 0.5,
                h: viewport.h,
            },
            SplitDir::Vertical => Rect {
                x: viewport.x,
                y: viewport.y + viewport.h * 0.5,
                w: viewport.w,
                h: viewport.h * 0.5,
            },
        };

        let pane = Pane::new(new_id, half_rect, cell_w, cell_h, shell)?;
        self.panes.insert(new_id, pane);

        let tab = &mut self.tabs[self.active_tab];
        tab.layout.split_pane(focused, new_id, direction);

        info!(focused, new_id, ?direction, "pane split");
        Ok(())
    }

    /// Close the focused pane. If it's the last pane in the tab, close the tab.
    /// Returns false if no tabs remain.
    pub fn close_focused_pane(&mut self) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        let tab = &mut self.tabs[self.active_tab];
        let focused = tab.focused_pane;
        let pane_ids = tab.layout.pane_ids();

        if pane_ids.len() <= 1 {
            // Last pane in tab → close tab
            return self.close_active_tab();
        }

        // Remove from layout
        if let Some(replacement) = tab.layout.remove_pane(focused) {
            tab.layout = replacement;
        }

        // Remove pane
        self.panes.remove(&focused);

        // Focus the first remaining pane
        let tab = &mut self.tabs[self.active_tab];
        let remaining = tab.layout.pane_ids();
        if let Some(&first) = remaining.first() {
            tab.focused_pane = first;
        }

        true
    }

    /// Get the focused pane of the active tab (immutable).
    pub fn focused_pane(&self) -> Option<&Pane> {
        let tab = self.tabs.get(self.active_tab)?;
        self.panes.get(&tab.focused_pane)
    }

    /// Get the focused pane of the active tab.
    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let tab = self.tabs.get(self.active_tab)?;
        self.panes.get_mut(&tab.focused_pane)
    }

    /// Get the focused pane ID.
    pub fn focused_pane_id(&self) -> Option<usize> {
        self.tabs.get(self.active_tab).map(|t| t.focused_pane)
    }

    /// Cycle focus to the next pane in the active tab.
    pub fn focus_next_pane(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            let ids = tab.layout.pane_ids();
            if let Some(pos) = ids.iter().position(|&id| id == tab.focused_pane) {
                let next = (pos + 1) % ids.len();
                tab.focused_pane = ids[next];
            }
        }
    }

    /// Compute viewports for the active tab given the available rect.
    /// Updates pane viewports and resizes grids/PTYs as needed.
    pub fn layout_active_tab(&mut self, available: Rect, cell_w: f32, cell_h: f32, gutter_cols: usize) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let mut viewports = Vec::new();
            tab.layout.compute_viewports(available, &mut viewports);
            for (pane_id, rect) in viewports {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.resize(rect, cell_w, cell_h, gutter_cols);
                }
            }
        }
    }

    /// Poll all panes in the active tab for PTY output. Returns true if any received data.
    pub fn poll_active_tab(&mut self) -> bool {
        let pane_ids: Vec<usize> = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.layout.pane_ids())
            .unwrap_or_default();

        let mut dirty = false;
        for id in pane_ids {
            if let Some(pane) = self.panes.get_mut(&id) {
                if pane.poll_output() {
                    dirty = true;
                }
            }
        }
        dirty
    }

    /// Get all visible panes with their viewports for rendering.
    pub fn visible_panes(&self) -> Vec<(&Pane, usize)> {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return Vec::new();
        };
        tab.layout
            .pane_ids()
            .into_iter()
            .filter_map(|id| self.panes.get(&id).map(|p| (p, id)))
            .collect()
    }
}
