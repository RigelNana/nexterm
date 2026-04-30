//! A single terminal pane: owns PTY (local or SSH), VTE parser, and I/O channel.

use crate::ssh_backend::{SftpCommand, SftpResult, SshSession};
use nexterm_pty::{LocalPty, PtyConfig};
use nexterm_sftp::RemoteEntry;
use nexterm_ssh::SshProfile;
use nexterm_vte::parser::TerminalParser;
use std::io::{Read, Write};
use std::sync::mpsc;
use tokio::runtime::Runtime;
use tracing::info;

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
    },
    Ssh {
        session: SshSession,
    },
}

/// A single terminal pane.
pub struct Pane {
    pub id: usize,
    pub terminal: TerminalParser,
    pub output_rx: mpsc::Receiver<Vec<u8>>,
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
    /// When true, output is buffered and filtered until the injection sentinel is seen.
    pub filtering_init: bool,
    /// Buffer for output received during init filtering.
    pub init_buf: Vec<u8>,
    /// SFTP browser state (SSH panes only).
    pub sftp_state: SftpBrowserState,
}

impl Pane {
    /// Spawn a new local PTY pane. `shell`: None = auto-detect, Some("wsl") = WSL, etc.
    pub fn new(
        id: usize,
        viewport: Rect,
        cell_w: f32,
        cell_h: f32,
        shell: Option<&str>,
    ) -> anyhow::Result<Self> {
        let cols = (viewport.w / cell_w).floor() as usize;
        let rows = (viewport.h / cell_h).floor() as usize;
        let cols = cols.max(1);
        let rows = rows.max(1);

        let terminal = TerminalParser::new(cols, rows);

        let pty = LocalPty::spawn(PtyConfig {
            shell: shell.map(|s| s.to_string()),
            cols: cols as u16,
            rows: rows as u16,
            ..Default::default()
        })?;

        let reader = pty.reader()?;
        let writer = pty.writer()?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                pty_reader_thread(reader, tx);
            })
            .expect("failed to spawn PTY reader thread");

        info!(id, cols, rows, "local pane spawned");

        Ok(Self {
            id,
            terminal,
            output_rx: rx,
            kind: PaneKind::Local { pty, writer },
            viewport,
            cols,
            rows,
            title: format!("Terminal {}", id + 1),
            exited: false,
            filtering_init: false,
            init_buf: Vec::new(),
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
    ) -> Self {
        let cols = (viewport.w / cell_w).floor() as usize;
        let rows = (viewport.h / cell_h).floor() as usize;
        let cols = cols.max(1);
        let rows = rows.max(1);

        let terminal = TerminalParser::new(cols, rows);
        let host = profile.host.clone();

        let (output_rx, session) =
            crate::ssh_backend::spawn_ssh_pane(rt, profile, cols as u16, rows as u16);

        info!(id, host = %host, cols, rows, "SSH pane spawned");

        Self {
            id,
            terminal,
            output_rx,
            kind: PaneKind::Ssh { session },
            viewport,
            cols,
            rows,
            title: format!("SSH: {}", host),
            exited: false,
            filtering_init: false,
            init_buf: Vec::new(),
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
                PaneKind::Ssh { session } => {
                    session.resize(new_cols as u16, new_rows as u16);
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
        const MAX_BYTES_PER_FRAME: usize = 4 * 1024 * 1024;
        const SENTINEL: &[u8] = b"\n__NEXTERM_INIT_DONE__";
        let mut received = false;
        let mut total = 0usize;
        while total < MAX_BYTES_PER_FRAME {
            match self.output_rx.try_recv() {
                Ok(data) => {
                    total += data.len();
                    if self.filtering_init {
                        self.init_buf.extend_from_slice(&data);
                        if let Some(pos) = self.init_buf.windows(SENTINEL.len()).position(|w| w == SENTINEL) {
                            let after = pos + SENTINEL.len();
                            let start = self.init_buf[after..].iter()
                                .position(|&b| b != b'\r' && b != b'\n')
                                .map(|p| after + p)
                                .unwrap_or(self.init_buf.len());
                            if start < self.init_buf.len() {
                                let remainder = self.init_buf[start..].to_vec();
                                self.terminal.process(&remainder);
                            }
                            self.filtering_init = false;
                            self.init_buf.clear();
                        } else if self.init_buf.len() > 64 * 1024 {
                            // Safety: if buffer grows too large, give up filtering
                            self.terminal.process(&std::mem::take(&mut self.init_buf));
                            self.filtering_init = false;
                        }
                    } else {
                        self.terminal.process(&data);
                    }
                    received = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.exited = true;
                    break;
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
        let replies = std::mem::take(&mut self.terminal.state.pending_replies);
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
            PaneKind::Ssh { session } => {
                session.write(data);
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
        if matches!(self.kind, PaneKind::Ssh { .. }) {
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
        self.write_to_pty(inject.as_bytes());
        self.filtering_init = true;
        self.init_buf.clear();
        info!(id = self.id, "shell integration injected via stdin (filtering until sentinel)");
    }

    /// Returns true if this is an SSH pane.
    pub fn is_ssh(&self) -> bool {
        matches!(self.kind, PaneKind::Ssh { .. })
    }

    /// Get a snapshot of the server status (SSH panes only).
    pub fn server_status(&self) -> Option<crate::ssh_backend::ServerStatus> {
        match &self.kind {
            PaneKind::Ssh { session } => Some(session.status.lock().unwrap().clone()),
            _ => None,
        }
    }

    /// Poll for SFTP results and update the browser state. Returns true if state changed.
    pub fn poll_sftp(&mut self) -> bool {
        let session = match &self.kind {
            PaneKind::Ssh { session } => session,
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
                SftpResult::DownloadDone { remote_path, local_path } => {
                    info!("SFTP download complete: {} → {}", remote_path, local_path);
                    if let Some(t) = self.sftp_state.transfers.iter_mut()
                        .find(|t| t.remote_path == remote_path)
                    {
                        t.done = true;
                        if t.total > 0 {
                            t.bytes = t.total;
                        }
                    }
                    let _ = local_path;
                }
                SftpResult::UploadDone { local_path, remote_path } => {
                    info!("SFTP upload complete: {} → {}", local_path, remote_path);
                    if let Some(t) = self.sftp_state.transfers.iter_mut()
                        .find(|t| t.remote_path == remote_path)
                    {
                        t.done = true;
                        if t.total > 0 {
                            t.bytes = t.total;
                        }
                    }
                }
                SftpResult::TransferProgress { dir, key, bytes, total } => {
                    if let Some(t) = self.sftp_state.transfers.iter_mut()
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
        if let PaneKind::Ssh { session } = &self.kind {
            session.sftp_command(SftpCommand::ListDir(path.to_string()));
        }
    }

    /// Navigate SFTP browser up one level.
    pub fn sftp_go_up(&self) {
        if let Some(parent) = std::path::Path::new(&self.sftp_state.current_path).parent() {
            let parent = parent.to_string_lossy().to_string();
            let parent = if parent.is_empty() { "/".to_string() } else { parent };
            self.sftp_navigate(&parent);
        }
    }

    /// Navigate SFTP browser to home.
    pub fn sftp_go_home(&self) {
        if let PaneKind::Ssh { session } = &self.kind {
            session.sftp_command(SftpCommand::GoHome);
        }
    }

    /// Create a remote directory.
    pub fn sftp_mkdir(&self, path: &str) {
        if let PaneKind::Ssh { session } = &self.kind {
            session.sftp_command(SftpCommand::Mkdir(path.to_string()));
        }
    }

    /// Create a remote empty file.
    pub fn sftp_touch(&self, path: &str) {
        if let PaneKind::Ssh { session } = &self.kind {
            session.sftp_command(SftpCommand::Touch(path.to_string()));
        }
    }

    /// Download a remote file to a local path.
    pub fn sftp_download(&mut self, remote_path: &str, local_path: &str) {
        if let PaneKind::Ssh { session } = &self.kind {
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
        if let PaneKind::Ssh { session } = &self.kind {
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
        self.sftp_state.transfers.retain(|t| !t.done && t.error.is_none());
    }
}

fn pty_reader_thread(mut reader: Box<dyn Read + Send>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 65536];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}
