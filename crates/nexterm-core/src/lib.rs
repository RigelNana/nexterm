//! # nexterm-core
//!
//! Core module for NexTerm: event bus, pane/tab management, application lifecycle.

pub mod event;
pub mod pane;
pub mod tab;

/// Application-wide unique identifier.
pub type PaneId = uuid::Uuid;
pub type TabId = uuid::Uuid;
pub type SessionId = uuid::Uuid;
