//! Local subprocess [`DockerBackend`] — spawns the `docker` CLI on the host
//! running NexTerm.
//!
//! Stateless aside from the binary name. All container operations shell out
//! to `docker`; logs stream through piped stdio; `exec -it` runs inside a
//! real local PTY via [`portable_pty`].

use std::io::{self, Read, Write};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::backend::{DockerBackend, LogStream, PtyIo};
use crate::model::ContainerInfo;
use crate::parse::parse_ps_lines;

/// Apply `CREATE_NO_WINDOW` on Windows so spawning `docker.exe` from a
/// GUI-subsystem binary doesn't flash a console window. No-op elsewhere.
///
/// `tokio::process::Command::creation_flags` is an inherent method on
/// Windows in current tokio versions — no `CommandExt` import needed.
#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

/// Runs `docker` as a subprocess on the local machine.
#[derive(Debug, Clone)]
pub struct LocalDockerBackend {
    docker_bin: String,
}

impl Default for LocalDockerBackend {
    fn default() -> Self {
        Self {
            docker_bin: "docker".into(),
        }
    }
}

impl LocalDockerBackend {
    /// Use `docker` from PATH.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the `docker` binary path (or name — e.g. `"podman"`).
    pub fn with_binary(path: impl Into<String>) -> Self {
        Self {
            docker_bin: path.into(),
        }
    }

    /// Run `docker <args>`, discard stdout, return () on success.
    async fn run_void(&self, args: &[&str]) -> Result<()> {
        let mut cmd = Command::new(&self.docker_bin);
        cmd.args(args).stdin(Stdio::null());
        hide_console(&mut cmd);
        let output = cmd
            .output()
            .await
            .with_context(|| format!("failed to spawn `{} {}`", self.docker_bin, args.join(" ")))?;
        if !output.status.success() {
            anyhow::bail!(
                "docker {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Run `docker <args>` and return captured stdout as UTF-8.
    async fn run_stdout(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.docker_bin);
        cmd.args(args).stdin(Stdio::null());
        hide_console(&mut cmd);
        let output = cmd
            .output()
            .await
            .with_context(|| format!("failed to spawn `{} {}`", self.docker_bin, args.join(" ")))?;
        if !output.status.success() {
            anyhow::bail!(
                "docker {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout).context("docker stdout was not valid UTF-8")
    }
}

#[async_trait]
impl DockerBackend for LocalDockerBackend {
    async fn probe(&self) -> Result<String> {
        let out = self
            .run_stdout(&["version", "--format", "{{.Server.Version}}"])
            .await?;
        Ok(out.trim().to_string())
    }

    async fn list(&self, all: bool) -> Result<Vec<ContainerInfo>> {
        // `--no-trunc` gives full 64-char IDs; JSON-lines is parsed by
        // `crate::parse_ps_lines`.
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
        serde_json::from_str(&out).context("failed to parse `docker inspect` JSON")
    }

    async fn logs_stream(&self, id: &str, tail: usize, follow: bool) -> Result<LogStream> {
        let tail_s = tail.to_string();
        let mut cmd = Command::new(&self.docker_bin);
        cmd.arg("logs").arg("--tail").arg(&tail_s);
        if follow {
            cmd.arg("-f");
        }
        cmd.arg(id)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_console(&mut cmd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `docker logs {id}`"))?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Byte-preserving: send Vec<u8> chunks exactly as Docker emits them
        // so the UI VTE can parse ANSI escape sequences intact.
        let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

        tokio::spawn(pump_async_reader(stdout, tx.clone()));
        tokio::spawn(pump_async_reader(stderr, tx));

        // Cancel-or-natural-exit supervisor.
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_rx => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                _ = child.wait() => {
                    // child exited on its own — nothing to clean up
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
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open local pty")?;

        let mut cmd = CommandBuilder::new(&self.docker_bin);
        cmd.arg("exec");
        cmd.arg("-it");
        cmd.arg(id);
        cmd.arg(shell);

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn `docker exec -it {id} {shell}`"))?;
        // Slave fd must be closed in the parent so read() returns EOF when
        // the child exits; only the child keeps the slave side open.
        drop(pair.slave);

        let pty = LocalPty::new(pair.master, child)?;
        Ok(Box::new(pty))
    }
}

/// Pump an async reader into an mpsc channel as byte chunks. Terminates on
/// EOF, read error, or when the receiver is dropped.
async fn pump_async_reader<R: tokio::io::AsyncRead + Unpin>(mut r: R, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// LocalPty — bridges a blocking `portable_pty` master into our async `PtyIo`.
// ---------------------------------------------------------------------------

struct LocalPty {
    /// Protected so we can call blocking write() from a `spawn_blocking` task
    /// without moving the writer out of the struct.
    writer: Arc<StdMutex<Box<dyn Write + Send>>>,
    /// Retained purely so `resize()` keeps working.
    master: Box<dyn MasterPty + Send>,
    /// Byte chunks pumped in by the blocking reader thread below.
    rx: mpsc::Receiver<Vec<u8>>,
    /// Unconsumed tail of the last chunk (when caller's buffer was smaller
    /// than the chunk we received).
    leftover: Vec<u8>,
    /// Keeps the child alive until drop. Also used by `Drop` to kill it.
    child: Box<dyn Child + Send + Sync>,
}

impl LocalPty {
    fn new(master: Box<dyn MasterPty + Send>, child: Box<dyn Child + Send + Sync>) -> Result<Self> {
        let reader = master.try_clone_reader().context("pty try_clone_reader")?;
        let writer = master.take_writer().context("pty take_writer")?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>(64);

        // Dedicated thread: portable_pty's reader is blocking, so we can't
        // park it on a tokio worker.
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            writer: Arc::new(StdMutex::new(writer)),
            master,
            rx,
            leftover: Vec::new(),
            child,
        })
    }
}

#[async_trait]
impl PtyIo for LocalPty {
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
        let writer = self.writer.clone();
        let data = buf.to_vec();
        let join = tokio::task::spawn_blocking(move || {
            let mut w = writer
                .lock()
                .map_err(|_| io::Error::other("pty writer mutex poisoned"))?;
            w.write(&data)
        })
        .await;
        match join {
            Ok(inner) => inner,
            Err(e) => Err(io::Error::other(e)),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        // Best-effort kill + reap. If the child already exited these are
        // both cheap no-ops.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Surface test: constructing and using the default binary name should
    /// at least not panic. Guarantees the struct stays Debug/Clone/Default.
    #[test]
    fn builds_default() {
        let b = LocalDockerBackend::new();
        assert_eq!(b.docker_bin, "docker");
        let b2 = LocalDockerBackend::with_binary("podman");
        assert_eq!(b2.docker_bin, "podman");
    }

    /// Run-through against a real docker daemon. Ignored by default so
    /// `cargo test` stays green in environments without docker — run with
    /// `cargo test -p nexterm-docker -- --ignored` on a machine that has
    /// docker installed and reachable.
    #[tokio::test]
    #[ignore = "needs a working docker daemon"]
    async fn probe_against_real_docker() {
        let backend = LocalDockerBackend::new();
        let version = backend.probe().await.expect("docker version");
        assert!(!version.is_empty(), "server version should be non-empty");
    }

    #[tokio::test]
    #[ignore = "needs a working docker daemon"]
    async fn list_all_against_real_docker() {
        let backend = LocalDockerBackend::new();
        let containers = backend.list(true).await.expect("docker ps -a");
        // Just assert it parses — we can't assume any specific containers.
        for c in &containers {
            assert!(!c.id.is_empty());
            assert!(!c.image.is_empty());
        }
    }
}
