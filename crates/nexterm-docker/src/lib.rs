//! Docker container management for NexTerm.
//!
//! Provides a [`DockerBackend`] trait with (future) Local-subprocess and
//! SSH-channel implementations. All backends shell out to the `docker` CLI
//! and parse `docker ps --format '{{json .}}'` into strongly-typed
//! [`ContainerInfo`] values.
//!
//! # Layout
//!
//! * [`model`] — data types: [`ContainerInfo`], [`ContainerStatus`],
//!   [`PortMapping`].
//! * [`parse`] — parser for `docker ps` JSON-lines output.
//! * [`backend`] — the async trait backends implement, plus [`LogStream`] /
//!   [`PtyIo`] helpers.
//!
//! The concrete `LocalDockerBackend` and `SshDockerBackend` live in sibling
//! modules added in a later step.

pub mod backend;
pub mod local;
pub mod model;
pub mod parse;
pub mod ssh;

pub use backend::{DockerBackend, LogStream, PtyIo};
pub use local::LocalDockerBackend;
pub use model::{ContainerInfo, ContainerStatus, PortMapping};
pub use parse::parse_ps_lines;
pub use ssh::SshDockerBackend;
