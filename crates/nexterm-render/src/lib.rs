//! # nexterm-render
//!
//! GPU-accelerated terminal renderer using `wgpu`.
//!
//! Responsibilities:
//! - Glyph atlas management (cosmic-text → texture atlas)
//! - Terminal grid → GPU vertex buffer conversion
//! - Shader pipeline: background fill, glyph rendering, cursor, selection
//! - 60 FPS+ with dirty-region tracking

pub mod atlas;
pub mod builtin_font;
pub mod gui;
pub mod pipeline;
pub mod renderer;
pub mod text_renderer;
