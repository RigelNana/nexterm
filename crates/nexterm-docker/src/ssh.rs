//! Remote [`DockerBackend`] — runs `docker` on the other side of an
//! established russh session.
//!
//! The backend never touches the interactive shell channel. Every operation
//! opens a fresh exec channel on the shared [`russh::client::Handle`], so
//! `docker ps` / `docker logs -f` / `docker exec -it` never race against
//! whatever the user is typing.
//!
//! The concrete `Handler` type is left generic so this crate stays free of
//! any nexterm-ssh dependency; the app layer specifies it when constructing
//! the backend (e.g. `SshDockerBackend::<ClientHandler>::new(handle)`).

use std::io;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use russh::ChannelMsg;
use russh::client::{Handle, Handler};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task;

use crate::backend::{DockerBackend, LogStream, PtyIo};
use crate::model::ContainerInfo;
use crate::parse::parse_ps_lines;

/// Runs `docker` on the remote end of an existing russh session.
///
/// Cheap to clone — the inner handle is already an `Arc`.
pub struct SshDockerBackend<H: Handler + Send + Sync + 'static> {
    handle: Arc<Handle<H>>,
    docker_bin: String,
}

impl<H: Handler + Send + Sync + 'static> SshDockerBackend<H> {
    /// Create a new backend bound to an existing russh session handle.
    pub fn new(handle: Arc<Handle<H>>) -> Self {
        Self {
            handle,
            docker_bin: "docker".into(),
        }
    }

    /// Override the remote docker binary (e.g. `"sudo docker"` or `"podman"`).
    pub fn with_binary(handle: Arc<Handle<H>>, binary: impl Into<String>) -> Self {
        Self {
            handle,
            docker_bin: binary.into(),
        }
    }

    /// Quote an argument for a POSIX shell. We feed the full command line to
    /// `docker` via a single `exec` request, so any container ID with
    /// surprising characters has to be escaped.
    fn shell_quote(s: &str) -> String {
        if s.is_empty() {
            return "''".into();
        }
        // Conservative: wrap in single quotes and escape any literal quotes.
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            if c == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(c);
            }
        }
        out.push('\'');
        out
    }

    fn build_cmd(&self, args: &[&str]) -> String {
        let mut s = String::with_capacity(self.docker_bin.len() + 32);
        s.push_str(&self.docker_bin);
        for a in args {
            s.push(' ');
            s.push_str(&Self::shell_quote(a));
        }
        s
    }

    /// Run `docker <args>` to completion, collecting stdout/stderr and exit
    /// status. Returns an error if stderr has content and the exit status is
    /// non-zero — mirrors the local backend.
    async fn run_collected(&self, args: &[&str]) -> Result<Vec<u8>> {
        let cmd = self.build_cmd(args);
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .context("failed to open ssh exec channel")?;
        channel
            .exec(true, cmd.as_bytes())
            .await
            .context("failed to send ssh exec request")?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit: Option<u32> = None;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { ref data }) => stdout.extend_from_slice(data),
                Some(ChannelMsg::ExtendedData { ref data, ext: 1 }) => {
                    stderr.extend_from_slice(data);
                }
                Some(ChannelMsg::ExtendedData { .. }) => { /* other extended types: ignore */ }
                Some(ChannelMsg::ExitStatus { exit_status }) => exit = Some(exit_status),
                Some(ChannelMsg::Eof) | None => break,
                _ => {}
            }
        }
        match exit {
            Some(0) | None => Ok(stdout),
            Some(code) => anyhow::bail!(
                "docker {} exited with {} on remote: {}",
                args.join(" "),
                code,
                String::from_utf8_lossy(&stderr).trim()
            ),
        }
    }

    async fn run_void(&self, args: &[&str]) -> Result<()> {
        self.run_collected(args).await.map(|_| ())
    }

    async fn run_stdout(&self, args: &[&str]) -> Result<String> {
        let bytes = self.run_collected(args).await?;
        String::from_utf8(bytes).context("docker stdout was not valid UTF-8")
    }
}

#[async_trait]
impl<H: Handler + Send + Sync + 'static> DockerBackend for SshDockerBackend<H> {
    async fn probe(&self) -> Result<String> {
        let out = self
            .run_stdout(&["version", "--format", "{{.Server.Version}}"])
            .await?;
        Ok(out.trim().to_string())
    }

    async fn list(&self, all: bool) -> Result<Vec<ContainerInfo>> {
        let mut args: Vec<&str> = vec!["ps", "--no-trunc", "--format", "{{json .}}"];
        if all {
            args.push("-a");
        }
        let out = self.run_stdout(&args).await?;
        parse_ps_lines(&out)
    }

    async fn start(&self, id: &str) -> Result<()> {
        self.run_void(&["start", id]).await
    }

    async fn stop(&self, id: &str, timeout_secs: u32) -> Result<()> {
        let t = timeout_secs.to_string();
        self.run_void(&["stop", "-t", &t, id]).await
    }

    async fn restart(&self, id: &str) -> Result<()> {
        self.run_void(&["restart", id]).await
    }

    async fn pause(&self, id: &str) -> Result<()> {
        self.run_void(&["pause", id]).await
    }

    async fn unpause(&self, id: &str) -> Result<()> {
        self.run_void(&["unpause", id]).await
    }

    async fn remove(&self, id: &str, force: bool, remove_volumes: bool) -> Result<()> {
        let mut args: Vec<&str> = vec!["rm"];
        if force {
            args.push("-f");
        }
        if remove_volumes {
            args.push("-v");
        }
        args.push(id);
        self.run_void(&args).await
    }

    async fn inspect(&self, id: &str) -> Result<Value> {
        let out = self.run_stdout(&["inspect", id]).await?;
        serde_json::from_str(&out).context("failed to parse remote `docker inspect` JSON")
    }

    async fn logs_stream(&self, id: &str, tail: usize, follow: bool) -> Result<LogStream> {
        let tail_s = tail.to_string();
        let mut args: Vec<&str> = vec!["logs", "--tail", &tail_s];
        if follow {
            args.push("-f");
        }
        args.push(id);
        let cmd = self.build_cmd(&args);

        let channel = self
            .handle
            .channel_open_session()
            .await
            .context("failed to open ssh exec channel for docker logs")?;
        channel
            .exec(true, cmd.as_bytes())
            .await
            .context("failed to send docker logs exec")?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();

        task::spawn(async move {
            let mut channel = channel;
            loop {
                tokio::select! {
                    msg = channel.wait() => {
                        match msg {
                            Some(ChannelMsg::Data { ref data }) => {
                                if tx.send(data.to_vec()).await.is_err() {
                                    break;
                                }
                            }
                            Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                                if tx.send(data.to_vec()).await.is_err() {
                                    break;
                                }
                            }
                            Some(ChannelMsg::Eof) | None => break,
                            _ => {}
                        }
                    }
                    _ = &mut cancel_rx => {
                        // Close the channel. russh's Drop already sends a
                        // close; explicit eof()/close() is best-effort.
                        let _ = channel.eof().await;
                        break;
                    }
                }
            }
        });

        Ok(LogStream {
            rx,
            cancel: cancel_tx,
        })
    }

    async fn exec_pty(
        &self,
        id: &str,
        shell: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Box<dyn PtyIo>> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .context("failed to open ssh exec channel for docker exec")?;
        channel
            .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
            .await
            .context("failed to request pty on remote")?;

        // `docker exec -it <id> <shell>` — the -i/-t are redundant under a
        // real PTY but harmless and keep parity with the local backend.
        let args: Vec<&str> = vec!["exec", "-i", "-t", id, shell];
        let cmd = self.build_cmd(&args);
        channel
            .exec(false, cmd.as_bytes())
            .await
            .context("failed to send docker exec exec request")?;

        Ok(Box::new(SshPty::new(channel)))
    }
}

// ------------------------------------------------------------
// SshPty — bridges a russh Channel to the `PtyIo` trait.
// ------------------------------------------------------------
//
// russh 0.46 doesn't expose a split read/write half (every Channel method
// needs `&mut self` for receiving), so we own the channel in a single pump
// task and use mpsc to expose async read/write/resize to the outside.

struct SshPty {
    /// Bytes typed by the user, forwarded to the channel's `data()` call by
    /// the pump task.
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Bytes from the remote process pushed in by the pump task.
    rx: mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
    /// Resize requests fanned to the pump task.
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
}

impl SshPty {
    fn new(channel: russh::Channel<russh::client::Msg>) -> Self {
        let (read_tx, rx) = mpsc::channel::<Vec<u8>>(64);
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();

        tokio::spawn(async move {
            let mut channel = channel;
            loop {
                tokio::select! {
                    msg = channel.wait() => {
                        match msg {
                            Some(ChannelMsg::Data { ref data }) => {
                                if read_tx.send(data.to_vec()).await.is_err() {
                                    break;
                                }
                            }
                            Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                                if read_tx.send(data.to_vec()).await.is_err() {
                                    break;
                                }
                            }
                            Some(ChannelMsg::Eof) | None => break,
                            _ => {}
                        }
                    }
                    Some(bytes) = write_rx.recv() => {
                        if channel.data(&bytes[..]).await.is_err() {
                            break;
                        }
                    }
                    Some((cols, rows)) = resize_rx.recv() => {
                        // Best-effort window-change request; ignore errors.
                        let _ = channel
                            .window_change(cols as u32, rows as u32, 0, 0)
                            .await;
                    }
                }
            }
        });

        Self {
            write_tx,
            rx,
            leftover: Vec::new(),
            resize_tx,
        }
    }
}

#[async_trait]
impl PtyIo for SshPty {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.leftover.is_empty() {
            let n = buf.len().min(self.leftover.len());
            buf[..n].copy_from_slice(&self.leftover[..n]);
            self.leftover.drain(..n);
            return Ok(n);
        }
        match self.rx.recv().await {
            Some(chunk) => {
                let n = buf.len().min(chunk.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.leftover = chunk[n..].to_vec();
                }
                Ok(n)
            }
            None => Ok(0),
        }
    }

    async fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_tx
            .send(buf.to_vec())
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(buf.len())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.resize_tx
            .send((cols, rows))
            .map_err(|e| io::Error::other(e.to_string()))
    }
}
