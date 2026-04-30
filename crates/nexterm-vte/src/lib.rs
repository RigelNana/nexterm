//! # nexterm-vte
//!
//! Terminal emulation layer: parses VT sequences and maintains a character grid.

pub mod grid;
pub mod parser;

/// Re-export the VTE parser for direct use.
pub use vte;
