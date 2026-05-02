//! Async surface every Docker backend implements.
//!
//! Two concrete backends will live alongside this module:
//! * `LocalDockerBackend` — spawns `docker` as a subprocess on the local host.
//! * `SshDockerBackend`  — opens fresh channels on an existing russh session.
//!
//! Both produce byte-preserving log streams ([`LogStream`]) and PTY handles
//! ([`PtyIo`]) so the GUI can render logs through nexterm-vte and run
//! `docker exec -it` as a normal NexTerm pane.

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::model::ContainerInfo;

/// Handle for a live log stream created by [`DockerBackend::logs_stream`].
///
/// Byte chunks flow through `rx` exactly as Docker emitted them — ANSI
/// escape sequences are preserved so the receiver can feed them to a VTE
/// parser. Send `()` on `cancel` to stop the underlying `docker logs -f`
/// process / channel; dropping the [`LogStream`] also terminates the stream
/// because the cancel sender closes.
pub struct LogStream {
    pub rx: mpsc::Receiver<Vec<u8>>,
    pub cancel: oneshot::Sender<()>,
}

/// Bidirectional PTY-like handle for an interactive `docker exec -it`.
///
/// Kept intentionally small so it fits both `portable_pty` masters (local)
/// and russh channels with a PTY request (SSH). The GUI wraps this in a
/// `nexterm-core::PaneBackend` so exec shells appear as ordinary tabs.
#[async_trait]
pub trait PtyIo: Send + Unpin {
    /// Read available bytes from the PTY. Returns `0` on EOF.
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;

    /// Write bytes to the PTY (i.e. stdin of the exec process).
    async fn write(&mut self, buf: &[u8]) -> std::io::Result<usize>;

    /// Forward a terminal resize to the exec process.
    fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()>;
}

/// Container-management operations that are uniform across local and SSH
/// hosts. Every backend runs the `docker` CLI and parses its output — we
/// don't speak the Docker HTTP API directly (keeps deployment trivial).
#[async_trait]
pub trait DockerBackend: Send + Sync {
    /// Quick health probe. Returns the server version string on success so
    /// the UI can display it; used to gate the panel when `docker` is
    /// unavailable on the target host.
    async fn probe(&self) -> anyhow::Result<String>;

    /// Run `docker ps [-a] --no-trunc --format '{{json .}}'` and parse it.
    async fn list(&self, all: bool) -> anyhow::Result<Vec<ContainerInfo>>;

    async fn start(&self, id: &str) -> anyhow::Result<()>;
    async fn stop(&self, id: &str, timeout_secs: u32) -> anyhow::Result<()>;
    async fn restart(&self, id: &str) -> anyhow::Result<()>;
    async fn pause(&self, id: &str) -> anyhow::Result<()>;
    async fn unpause(&self, id: &str) -> anyhow::Result<()>;

    /// Remove a container. `force=true` kills a running container first;
    /// `remove_volumes=true` also removes anonymous volumes.
    async fn remove(&self, id: &str, force: bool, remove_volumes: bool) -> anyhow::Result<()>;

    /// `docker inspect <id>` as parsed JSON. The result is typically a
    /// single-element array; the caller is responsible for indexing into it.
    async fn inspect(&self, id: &str) -> anyhow::Result<Value>;

    /// Start streaming `docker logs --tail <tail> [-f] <id>`. Raw bytes are
    /// forwarded verbatim — keep ANSI sequences intact for VTE rendering.
    async fn logs_stream(&self, id: &str, tail: usize, follow: bool) -> anyhow::Result<LogStream>;

    /// Start `docker exec -it <id> <shell>` attached to a fresh PTY at the
    /// given geometry. The returned handle is owned by the caller and can be
    /// wrapped in a pane backend.
    async fn exec_pty(
        &self,
        id: &str,
        shell: &str,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Box<dyn PtyIo>>;
}
