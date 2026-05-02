//! A single terminal pane: owns PTY (local or SSH), VTE parser, and I/O channel.

use crate::Waker;
use crate::ssh_backend::{SftpCommand, SftpResult, SshSession};
use nexterm_docker::{DockerBackend, PtyIo};
use nexterm_pty::{LocalPty, PtyConfig};
use nexterm_sftp::RemoteEntry;
use nexterm_ssh::SshProfile;
use nexterm_vte::grid::Grid;
use nexterm_vte::parser::TerminalParser;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::info;

const READ_BUFFER_SIZE: usize = 0x10_0000;
const MAX_LOCKED_READ: usize = 4 * READ_BUFFER_SIZE;
const MAX_CHANNEL_BYTES_PER_FRAME: usize = 4 * 1024 * 1024;
const INIT_SENTINEL: &[u8] = b"\n__NEXTERM_INIT_DONE__";

#[derive(Clone)]
pub struct SharedTerminal {
    inner: Arc<Mutex<TerminalParser>>,
}

impl SharedTerminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TerminalParser::new(cols, rows))),
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, TerminalParser>> {
        self.inner.try_lock()
    }

    pub fn grid(&self) -> MappedMutexGuard<'_, Grid> {
        MutexGuard::map(self.inner.lock(), |terminal| terminal.grid_mut())
    }

    pub fn grid_mut(&self) -> MappedMutexGuard<'_, Grid> {
        self.grid()
    }

    /// Non-blocking counterpart to [`grid`].  Returns `None` if the PTY
    /// reader / VTE parser is currently holding the lock, so the caller
    /// can fall back to skipping the frame instead of blocking on the
    /// producer.  Used by the render path to avoid the previous
    /// workaround of cloning the entire grid (including scrollback, up
    /// to ~300 MiB with default settings) on every vblank.
    pub fn grid_try_lock(&self) -> Option<MappedMutexGuard<'_, Grid>> {
        self.inner
            .try_lock()
            .map(|guard| MutexGuard::map(guard, |t| t.grid_mut()))
    }

    pub fn process(&self, data: &[u8]) {
        self.inner.lock().process(data);
    }

    fn take_pending_replies(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.inner.lock().state.pending_replies)
    }
}

#[derive(Default)]
struct InitFilterState {
    filtering: bool,
    init_buf: Vec<u8>,
}

impl InitFilterState {
    fn start(&mut self) {
        self.filtering = true;
        self.init_buf.clear();
    }
}

enum OutputSource {
    Local {
        dirty: Arc<AtomicBool>,
        exited: Arc<AtomicBool>,
    },
    Bytes(mpsc::Receiver<Vec<u8>>),
}

/// One in-flight or completed SFTP file transfer (for UI display).
#[derive(Debug, Clone)]
pub struct TransferStatus {
    pub direction: crate::ssh_backend::TransferDir,
    /// Remote path (used as the unique key).
    pub remote_path: String,
    /// Local path (source for upload, destination for download).
    pub local_path: String,
    pub bytes: u64,
    pub total: u64,
    pub done: bool,
    pub error: Option<String>,
}

/// SFTP file browser state for a pane.
#[derive(Debug, Clone, Default)]
pub struct SftpBrowserState {
    /// Current remote directory path.
    pub current_path: String,
    /// Entries in the current directory.
    pub entries: Vec<RemoteEntry>,
    /// Expanded directory paths in the left tree view.
    pub expanded_dirs: std::collections::HashSet<String>,
    /// Children of expanded dirs: path → entries.
    pub tree_children: std::collections::HashMap<String, Vec<RemoteEntry>>,
    /// Whether the browser has been initialized.
    pub initialized: bool,
    /// Error message, if any.
    pub error: Option<String>,
    /// Active and recently-completed transfers, keyed by remote_path.
    pub transfers: Vec<TransferStatus>,
}

/// Pixel-space rectangle for viewport positioning.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Backend-specific state for a pane.
pub enum PaneKind {
    Local {
        pty: LocalPty,
        writer: Box<dyn Write + Send>,
        /// Shell path used to spawn this pane (for cloning on split).
        shell: Option<String>,
    },
    Ssh {
        session: SshSession,
        /// Profile used to connect (cloned when splitting).
        profile: SshProfile,
    },
    /// `docker exec -it` into a running container. I/O is funnelled through
    /// unbounded channels to an async task that owns the underlying
    /// [`PtyIo`] (which may be a local portable-pty master or an SSH
    /// channel). Bytes flow back on [`Pane::output_rx`] just like the
    /// other variants.
    DockerExec {
        write_tx: tokio_mpsc::UnboundedSender<Vec<u8>>,
        resize_tx: tokio_mpsc::UnboundedSender<(u16, u16)>,
        /// Display label (container name or id) — used as the tab title.
        label: String,
    },
}

/// A single terminal pane.
pub struct Pane {
    pub id: usize,
    pub terminal: SharedTerminal,
    output: OutputSource,
    pub kind: PaneKind,
    /// The pane's viewport in pixel coordinates.
    pub viewport: Rect,
    /// Grid dimensions (derived from viewport and cell size).
    pub cols: usize,
    pub rows: usize,
    /// Title (from OSC or default).
    pub title: String,
    /// True once the PTY/SSH child has exited (output channel disconnected).
    pub exited: bool,
    init_filter: Arc<Mutex<InitFilterState>>,
    /// SFTP browser state (SSH panes only).
    pub sftp_state: SftpBrowserState,
}

impl Pane {
    /// Spawn a new local PTY pane. `shell`: None = auto-detect, Some("wsl") = WSL, etc.
    /// `waker` is invoked from the PTY reader thread after each chunk arrives,
    /// so the main event loop wakes immediately instead of polling.
    pub fn new(
        id: usize,
        viewport: Rect,
        cell_w: f32,
        cell_h: f32,
        shell: Option<&str>,
        waker: Waker,
    ) -> anyhow::Result<Self> {
        let cols = (viewport.w / cell_w).floor() as usize;
        let rows = (viewport.h / cell_h).floor() as usize;
        let cols = cols.max(1);
        let rows = rows.max(1);

        let terminal = SharedTerminal::new(cols, rows);

        let pty = LocalPty::spawn(PtyConfig {
            shell: shell.map(|s| s.to_string()),
            cols: cols as u16,
            rows: rows as u16,
            ..Default::default()
        })?;

        let reader = pty.reader()?;
        let writer = pty.writer()?;

        let dirty = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::new(AtomicBool::new(false));
        let init_filter = Arc::new(Mutex::new(InitFilterState::default()));
        let reader_terminal = terminal.clone();
        let reader_dirty = dirty.clone();
        let reader_exited = exited_flag.clone();
        let reader_init_filter = init_filter.clone();
        std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                pty_reader_thread(
                    reader,
                    reader_terminal,
                    reader_dirty,
                    reader_exited,
                    reader_init_filter,
                    waker,
                );
            })
            .expect("failed to spawn PTY reader thread");

        info!(id, cols, rows, "local pane spawned");

        Ok(Self {
            id,
            terminal,
            output: OutputSource::Local {
                dirty,
                exited: exited_flag,
            },
            kind: PaneKind::Local {
                pty,
                writer,
                shell: shell.map(|s| s.to_string()),
            },
            viewport,
            cols,
            rows,
            title: format!("Terminal {}", id + 1),
            exited: false,
            init_filter,
            sftp_state: SftpBrowserState::default(),
        })
    }

    /// Spawn a new SSH pane connected to a remote host.
    pub fn new_ssh(
        id: usize,
        viewport: Rect,
        cell_w: f32,
        cell_h: f32,
        profile: SshProfile,
        rt: &Runtime,
        waker: Waker,
    ) -> Self {
        let cols = (viewport.w / cell_w).floor() as usize;
        let rows = (viewport.h / cell_h).floor() as usize;
        let cols = cols.max(1);
        let rows = rows.max(1);

        let terminal = SharedTerminal::new(cols, rows);
        let host = profile.host.clone();

        let profile_clone = profile.clone();
        let (output_rx, session) =
            crate::ssh_backend::spawn_ssh_pane(rt, profile, cols as u16, rows as u16, waker);

        info!(id, host = %host, cols, rows, "SSH pane spawned");

        Self {
            id,
            terminal,
            output: OutputSource::Bytes(output_rx),
            kind: PaneKind::Ssh {
                session,
                profile: profile_clone,
            },
            viewport,
            cols,
            rows,
            title: format!("SSH: {}", host),
            exited: false,
            init_filter: Arc::new(Mutex::new(InitFilterState::default())),
            sftp_state: SftpBrowserState::default(),
        }
    }

    /// Spawn a new docker-exec pane attached to a running container.
    ///
    /// `backend` may be either local or SSH; the pane doesn't need to know.
    /// The async `exec_pty` call happens inside a background task so this
    /// function returns immediately with an empty pane — any failure will
    /// surface as an error banner written into the terminal output before
    /// the channel closes.
    pub fn new_docker_exec(
        id: usize,
        viewport: Rect,
        cell_w: f32,
        cell_h: f32,
        backend: Arc<dyn DockerBackend>,
        container_id: String,
        container_label: String,
        shell: String,
        rt: &Runtime,
        waker: Waker,
    ) -> Self {
        let cols = ((viewport.w / cell_w).floor() as usize).max(1);
        let rows = ((viewport.h / cell_h).floor() as usize).max(1);

        let terminal = SharedTerminal::new(cols, rows);

        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
        let (write_tx, write_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = tokio_mpsc::unbounded_channel::<(u16, u16)>();

        // Fire the exec request and wire the io pump on the tokio runtime.
        // If `exec_pty` fails we write a single red error line into the
        // terminal and drop the output sender, which marks the pane as
        // exited on the next `poll_output` tick.
        let err_tx = output_tx.clone();
        let container_id_for_task = container_id.clone();
        let shell_for_task = shell.clone();
        let waker_for_task = waker.clone();
        let waker_for_err = waker.clone();
        rt.spawn(async move {
            match backend
                .exec_pty(
                    &container_id_for_task,
                    &shell_for_task,
                    cols as u16,
                    rows as u16,
                )
                .await
            {
                Ok(io) => {
                    docker_exec_io_task(io, output_tx, write_rx, resize_rx, waker_for_task).await;
                }
                Err(e) => {
                    // ANSI red + bold for visibility; ending with CRLF so
                    // any subsequent output stays on its own line.
                    let msg =
                        format!("\r\n\x1b[1;31mdocker exec failed:\x1b[0m {e}\r\n").into_bytes();
                    let _ = err_tx.send(msg);
                    waker_for_err();
                }
            }
        });

        info!(
            id,
            cols,
            rows,
            container = %container_id,
            label = %container_label,
            "docker exec pane spawned"
        );

        let title = format!("🐳 {}", container_label);
        Self {
            id,
            terminal,
            output: OutputSource::Bytes(output_rx),
            kind: PaneKind::DockerExec {
                write_tx,
                resize_tx,
                label: container_label,
            },
            viewport,
            cols,
            rows,
            title,
            exited: false,
            init_filter: Arc::new(Mutex::new(InitFilterState::default())),
            sftp_state: SftpBrowserState::default(),
        }
    }

    /// Resize this pane to a new viewport. Returns true if grid size changed.
    /// `gutter_cols` is the number of columns reserved for the log-mode gutter (0 if not in log mode).
    pub fn resize(&mut self, viewport: Rect, cell_w: f32, cell_h: f32, gutter_cols: usize) -> bool {
        self.viewport = viewport;
        let total_cols = (viewport.w / cell_w).floor() as usize;
        let new_cols = total_cols.saturating_sub(gutter_cols);
        let new_rows = (viewport.h / cell_h).floor() as usize;
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);

        if new_cols != self.cols || new_rows != self.rows {
            self.cols = new_cols;
            self.rows = new_rows;
            self.terminal.grid_mut().resize(new_cols, new_rows);
            match &mut self.kind {
                PaneKind::Local { pty, .. } => {
                    let _ = pty.resize(new_cols as u16, new_rows as u16);
                }
                PaneKind::Ssh { session, .. } => {
                    session.resize(new_cols as u16, new_rows as u16);
                }
                PaneKind::DockerExec { resize_tx, .. } => {
                    let _ = resize_tx.send((new_cols as u16, new_rows as u16));
                }
            }
            true
        } else {
            false
        }
    }

    /// Drain PTY/SSH output and feed into VTE parser. Returns true if anything was received.
    /// Drains all available data up to a byte cap; the render rate limiter in
    /// main.rs controls frame timing so no time budget is needed here.
    ///
    /// When `filtering_init` is true (during shell integration injection),
    /// output is buffered and only forwarded after the sentinel is seen.
    pub fn poll_output(&mut self) -> bool {
        let mut received = false;
        let mut total = 0usize;

        match &self.output {
            OutputSource::Local { dirty, exited } => {
                received = dirty.swap(false, Ordering::AcqRel);
                if exited.swap(false, Ordering::AcqRel) {
                    self.exited = true;
                }
            }
            OutputSource::Bytes(output_rx) => {
                while total < MAX_CHANNEL_BYTES_PER_FRAME {
                    match output_rx.try_recv() {
                        Ok(data) => {
                            total += data.len();
                            self.terminal.process(&data);
                            received = true;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            self.exited = true;
                            break;
                        }
                    }
                }
            }
        }
        // Send any terminal-to-host replies (DSR, DA, etc.) back through the PTY.
        self.flush_replies();

        // On Windows, ConPTY may not signal EOF immediately after the child
        // exits. Actively poll the child process to detect exit.
        if !self.exited {
            if let PaneKind::Local { pty, .. } = &mut self.kind {
                if let Ok(Some(_status)) = pty.try_wait() {
                    self.exited = true;
                }
            }
        }

        received
    }

    /// Write any queued terminal replies back to the PTY/SSH channel.
    fn flush_replies(&mut self) {
        let replies = self.terminal.take_pending_replies();
        for reply in &replies {
            self.write_to_pty(reply);
        }
    }

    /// Write bytes to the PTY/SSH channel.
    pub fn write_to_pty(&mut self, data: &[u8]) {
        match &mut self.kind {
            PaneKind::Local { writer, .. } => {
                let _ = writer.write_all(data);
                let _ = writer.flush();
            }
            PaneKind::Ssh { session, .. } => {
                session.write(data);
            }
            PaneKind::DockerExec { write_tx, .. } => {
                let _ = write_tx.send(data.to_vec());
            }
        }
    }

    /// Inject shell integration (OSC 133 prompt hooks) by writing commands to
    /// the PTY stdin. Used for WSL and other shells where we can't inject via
    /// command-line arguments. Does nothing for SSH panes (handled separately).
    ///
    /// Uses sentinel + clear: writes injection, echoes sentinel, then clears.
    /// `poll_output` filters all output until the sentinel is seen.
    pub fn inject_shell_integration(&mut self) {
        // Shell integration is only meaningful for locally-spawned shells.
        // SSH panes inject their own hooks over the channel, and docker
        // exec panes run arbitrary shells inside containers where the
        // hooks are neither needed nor safe to assume.
        if matches!(
            self.kind,
            PaneKind::Ssh { .. } | PaneKind::DockerExec { .. }
        ) {
            return;
        }
        let inject = concat!(
            " if [ -n \"$BASH_VERSION\" ]; then ",
            "PROMPT_COMMAND='printf \"\\033]133;A\\007\"'\"${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"; ",
            "PS1=\"$PS1\"'\\[\\033]133;B\\007\\]'; ",
            "elif [ -n \"$ZSH_VERSION\" ]; then ",
            "precmd() { printf '\\033]133;A\\007'; }; ",
            "preexec() { printf '\\033]133;B\\007'; }; ",
            "fi; echo __NEXTERM_INIT_DONE__; clear\n"
        );
        self.init_filter.lock().start();
        self.write_to_pty(inject.as_bytes());
        info!(
            id = self.id,
            "shell integration injected via stdin (filtering until sentinel)"
        );
    }

    /// Returns true if this is an SSH pane.
    pub fn is_ssh(&self) -> bool {
        matches!(self.kind, PaneKind::Ssh { .. })
    }

    /// Get the SSH profile if this is an SSH pane (for cloning on split).
    pub fn ssh_profile(&self) -> Option<SshProfile> {
        match &self.kind {
            PaneKind::Ssh { profile, .. } => Some(profile.clone()),
            _ => None,
        }
    }

    /// Get the shell path if this is a local pane (for cloning on split).
    pub fn local_shell(&self) -> Option<String> {
        match &self.kind {
            PaneKind::Local { shell, .. } => shell.clone(),
            _ => None,
        }
    }

    /// Get a snapshot of the server status (SSH panes only).
    pub fn server_status(&self) -> Option<crate::ssh_backend::ServerStatus> {
        match &self.kind {
            PaneKind::Ssh { session, .. } => Some(session.status.lock().unwrap().clone()),
            _ => None,
        }
    }

    /// Poll for SFTP results and update the browser state. Returns true if state changed.
    pub fn poll_sftp(&mut self) -> bool {
        let session = match &self.kind {
            PaneKind::Ssh { session, .. } => session,
            _ => return false,
        };
        let mut changed = false;
        while let Ok(result) = session.sftp_result_rx.try_recv() {
            match result {
                SftpResult::DirListing { path, entries } => {
                    self.sftp_state.current_path = path;
                    self.sftp_state.entries = entries;
                    self.sftp_state.initialized = true;
                    self.sftp_state.error = None;
                }
                SftpResult::Error(msg) => {
                    self.sftp_state.error = Some(msg);
                }
                SftpResult::DownloadDone {
                    remote_path,
                    local_path,
                } => {
                    info!("SFTP download complete: {} → {}", remote_path, local_path);
                    if let Some(t) = self
                        .sftp_state
                        .transfers
                        .iter_mut()
                        .find(|t| t.remote_path == remote_path)
                    {
                        t.done = true;
                        if t.total > 0 {
                            t.bytes = t.total;
                        }
                    }
                    let _ = local_path;
                }
                SftpResult::UploadDone {
                    local_path,
                    remote_path,
                } => {
                    info!("SFTP upload complete: {} → {}", local_path, remote_path);
                    if let Some(t) = self
                        .sftp_state
                        .transfers
                        .iter_mut()
                        .find(|t| t.remote_path == remote_path)
                    {
                        t.done = true;
                        if t.total > 0 {
                            t.bytes = t.total;
                        }
                    }
                }
                SftpResult::TransferProgress {
                    dir,
                    key,
                    bytes,
                    total,
                } => {
                    if let Some(t) = self
                        .sftp_state
                        .transfers
                        .iter_mut()
                        .find(|t| t.remote_path == key)
                    {
                        t.bytes = bytes;
                        t.total = total;
                    } else {
                        // First progress event for a new transfer — register it.
                        self.sftp_state.transfers.push(TransferStatus {
                            direction: dir,
                            remote_path: key,
                            local_path: String::new(),
                            bytes,
                            total,
                            done: false,
                            error: None,
                        });
                    }
                }
            }
            changed = true;
        }
        changed
    }

    /// Navigate the SFTP browser to a path.
    pub fn sftp_navigate(&self, path: &str) {
        if let PaneKind::Ssh { session, .. } = &self.kind {
            session.sftp_command(SftpCommand::ListDir(path.to_string()));
        }
    }

    /// Navigate SFTP browser up one level.
    pub fn sftp_go_up(&self) {
        if let Some(parent) = std::path::Path::new(&self.sftp_state.current_path).parent() {
            let parent = parent.to_string_lossy().to_string();
            let parent = if parent.is_empty() {
                "/".to_string()
            } else {
                parent
            };
            self.sftp_navigate(&parent);
        }
    }

    /// Navigate SFTP browser to home.
    pub fn sftp_go_home(&self) {
        if let PaneKind::Ssh { session, .. } = &self.kind {
            session.sftp_command(SftpCommand::GoHome);
        }
    }

    /// Create a remote directory.
    pub fn sftp_mkdir(&self, path: &str) {
        if let PaneKind::Ssh { session, .. } = &self.kind {
            session.sftp_command(SftpCommand::Mkdir(path.to_string()));
        }
    }

    /// Create a remote empty file.
    pub fn sftp_touch(&self, path: &str) {
        if let PaneKind::Ssh { session, .. } = &self.kind {
            session.sftp_command(SftpCommand::Touch(path.to_string()));
        }
    }

    /// Download a remote file to a local path.
    pub fn sftp_download(&mut self, remote_path: &str, local_path: &str) {
        if let PaneKind::Ssh { session, .. } = &self.kind {
            // Register transfer locally so UI shows it before first progress event.
            self.sftp_state.transfers.push(TransferStatus {
                direction: crate::ssh_backend::TransferDir::Download,
                remote_path: remote_path.to_string(),
                local_path: local_path.to_string(),
                bytes: 0,
                total: 0,
                done: false,
                error: None,
            });
            session.sftp_command(SftpCommand::Download {
                remote_path: remote_path.to_string(),
                local_path: local_path.to_string(),
            });
        }
    }

    /// Upload a local file to a remote path.
    pub fn sftp_upload(&mut self, local_path: &str, remote_path: &str) {
        if let PaneKind::Ssh { session, .. } = &self.kind {
            self.sftp_state.transfers.push(TransferStatus {
                direction: crate::ssh_backend::TransferDir::Upload,
                remote_path: remote_path.to_string(),
                local_path: local_path.to_string(),
                bytes: 0,
                total: 0,
                done: false,
                error: None,
            });
            session.sftp_command(SftpCommand::Upload {
                local_path: local_path.to_string(),
                remote_path: remote_path.to_string(),
            });
        }
    }

    /// Remove completed transfers from the visible list.
    pub fn sftp_clear_done_transfers(&mut self) {
        self.sftp_state
            .transfers
            .retain(|t| !t.done && t.error.is_none());
    }
}

fn pty_reader_thread(
    mut reader: Box<dyn Read + Send>,
    terminal: SharedTerminal,
    dirty: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    init_filter: Arc<Mutex<InitFilterState>>,
    waker: Waker,
) {
    let mut buf = [0u8; READ_BUFFER_SIZE];
    // Optional tracing: NEXTERM_PTY_TRACE=1 prints once a second how many
    // bytes/chunks the OS PTY layer (ConPTY on Windows) is delivering.
    // Useful for spotting ConPTY byte amplification on scroll-heavy
    // workloads where vtebench reports throughput in seconds-per-MiB.
    let trace = std::env::var("NEXTERM_PTY_TRACE")
        .ok()
        .filter(|v| v != "0")
        .is_some();
    let mut last_log = std::time::Instant::now();
    let mut bytes_window: u64 = 0;
    let mut chunks_window: u64 = 0;
    let wake_scheduled = Arc::new(AtomicBool::new(false));
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if trace {
                    bytes_window += n as u64;
                    chunks_window += 1;
                    let elapsed = last_log.elapsed();
                    if elapsed >= std::time::Duration::from_secs(1) {
                        let secs = elapsed.as_secs_f64();
                        let mbps = (bytes_window as f64) / secs / (1024.0 * 1024.0);
                        let avg_chunk = if chunks_window == 0 {
                            0
                        } else {
                            (bytes_window / chunks_window) as usize
                        };
                        info!(
                            target: "pty_trace",
                            bytes = bytes_window,
                            chunks = chunks_window,
                            avg_chunk,
                            mbps = format!("{mbps:.2}"),
                            "pty reader window"
                        );
                        bytes_window = 0;
                        chunks_window = 0;
                        last_log = std::time::Instant::now();
                    }
                }
                let filtered = {
                    let mut filter = init_filter.lock();
                    if !filter.filtering {
                        None
                    } else {
                        filter.init_buf.extend_from_slice(&buf[..n]);
                        if let Some(pos) = filter
                            .init_buf
                            .windows(INIT_SENTINEL.len())
                            .position(|w| w == INIT_SENTINEL)
                        {
                            let after = pos + INIT_SENTINEL.len();
                            let start = filter.init_buf[after..]
                                .iter()
                                .position(|&b| b != b'\r' && b != b'\n')
                                .map(|p| after + p)
                                .unwrap_or(filter.init_buf.len());
                            let remainder = if start < filter.init_buf.len() {
                                Some(filter.init_buf[start..].to_vec())
                            } else {
                                None
                            };
                            filter.filtering = false;
                            filter.init_buf.clear();
                            remainder
                        } else if filter.init_buf.len() > READ_BUFFER_SIZE {
                            filter.filtering = false;
                            Some(std::mem::take(&mut filter.init_buf))
                        } else {
                            continue;
                        }
                    }
                };

                if let Some(data) = filtered {
                    if data.is_empty() {
                        continue;
                    }
                    terminal.process(&data);
                } else {
                    terminal.process(&buf[..n]);
                }
                dirty.store(true, Ordering::Release);
                let delay = if n >= MAX_LOCKED_READ {
                    std::time::Duration::ZERO
                } else {
                    std::time::Duration::from_millis(2)
                };
                schedule_pty_wake(&waker, &wake_scheduled, delay);
            }
            Err(_) => break,
        }
    }
    // Final wake so the main loop notices the channel disconnected (pane exit).
    exited.store(true, Ordering::Release);
    waker();
}

fn schedule_pty_wake(waker: &Waker, scheduled: &Arc<AtomicBool>, delay: std::time::Duration) {
    if scheduled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let thread_waker = waker.clone();
    let fallback_waker = waker.clone();
    let thread_scheduled = scheduled.clone();
    let fallback_scheduled = scheduled.clone();
    let spawn_result = std::thread::Builder::new()
        .name("pty-wake-coalesce".into())
        .spawn(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            thread_scheduled.store(false, Ordering::Release);
            thread_waker();
        });

    if spawn_result.is_err() {
        fallback_scheduled.store(false, Ordering::Release);
        fallback_waker();
    }
}

/// I/O pump for a docker-exec pane. Multiplexes reads from the [`PtyIo`]
/// into the pane's output channel, and inbound writes/resizes from the
/// pane's channels back onto the PtyIo.
///
/// Exits when either side disconnects (user closed the tab, container
/// exited, or the underlying channel dropped).
async fn docker_exec_io_task(
    mut io: Box<dyn PtyIo>,
    output_tx: mpsc::Sender<Vec<u8>>,
    mut write_rx: tokio_mpsc::UnboundedReceiver<Vec<u8>>,
    mut resize_rx: tokio_mpsc::UnboundedReceiver<(u16, u16)>,
    waker: Waker,
) {
    let mut buf = vec![0u8; 8192];
    loop {
        tokio::select! {
            // Read is `&mut io` + `&mut buf`; the other branches never
            // touch io/buf so the borrow is exclusive for the lifetime of
            // the pending future. When another branch wins, the read
            // future is dropped (cancelling the borrow), we run that
            // branch's body, then loop back.
            res = io.read(&mut buf) => match res {
                Ok(0) => break,      // EOF — container's shell exited.
                Ok(n) => {
                    if output_tx.send(buf[..n].to_vec()).is_err() {
                        break; // receiver gone (pane closed)
                    }
                    waker();
                }
                Err(_) => break,
            },
            Some(data) = write_rx.recv() => {
                if io.write(&data).await.is_err() {
                    break;
                }
            }
            Some((cols, rows)) = resize_rx.recv() => {
                let _ = io.resize(cols, rows);
            }
            else => break,
        }
    }
    // Final wake so the main loop notices the channel disconnected.
    waker();
}
