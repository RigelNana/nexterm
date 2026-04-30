//! SSH backend: bridges async russh channel I/O to sync mpsc for the pane system.

use nexterm_sftp::RemoteEntry;
use nexterm_ssh::connection::SshConnection;
use nexterm_ssh::SshProfile;
use russh::ChannelMsg;
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Instant;
use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{error, info, warn};

/// SFTP command sent from UI to the SSH background task.
#[derive(Debug)]
pub enum SftpCommand {
    /// List directory contents at the given path.
    ListDir(String),
    /// Navigate to home directory.
    GoHome,
    /// Create a directory at the given path, then refresh listing.
    Mkdir(String),
    /// Create an empty file (touch) at the given path, then refresh listing.
    Touch(String),
    /// Download a remote file to a local path.
    Download { remote_path: String, local_path: String },
    /// Upload a local file to a remote path.
    Upload { local_path: String, remote_path: String },
}

/// Direction of an in-flight transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDir {
    Download,
    Upload,
}

/// SFTP result sent from the SSH background task to the UI.
#[derive(Debug, Clone)]
pub enum SftpResult {
    /// Directory listing result: (current_path, entries).
    DirListing { path: String, entries: Vec<RemoteEntry> },
    /// Error message.
    Error(String),
    /// Download completed.
    DownloadDone { remote_path: String, local_path: String },
    /// Upload completed.
    UploadDone { local_path: String, remote_path: String },
    /// Progress update for an in-flight transfer. `key` is the remote_path,
    /// used by the UI to look up the corresponding job.
    TransferProgress {
        dir: TransferDir,
        key: String,
        bytes: u64,
        total: u64,
    },
}

/// Per-disk usage info.
#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub mount: String,
    pub fstype: String,
    pub total: String,
    pub used: String,
    pub avail: String,
    pub use_pct: String,
}

/// Per-network-interface snapshot (cumulative bytes).
#[derive(Debug, Clone)]
pub struct NetIfSnapshot {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// One sample of network rates (bytes/sec), derived from two snapshots.
#[derive(Debug, Clone, Default)]
pub struct NetRateSample {
    pub rx_bps: f64,
    pub tx_bps: f64,
}

/// Live server status, updated periodically via a separate SSH exec channel.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    /// Remote OS name (e.g. "Linux", "Darwin").
    pub os: String,
    /// Remote kernel version (e.g. "6.1.0-18-amd64").
    pub kernel: String,
    /// Remote hostname.
    pub hostname: String,
    /// System uptime string.
    pub uptime: String,
    /// Load averages (1/5/15 min).
    pub load_avg: String,
    /// Total / used / free memory in MB.
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    /// CPU usage percentage (0-100).
    pub cpu_usage_pct: f32,
    /// Per-disk usage.
    pub disks: Vec<DiskInfo>,
    /// Disk usage of root partition (legacy, kept for status bar).
    pub disk_usage: String,
    /// Current network interface counters.
    pub net_ifs: Vec<NetIfSnapshot>,
    /// Recent aggregate network rate history (newest last), up to 60 samples.
    pub net_history: Vec<NetRateSample>,
    /// Per-interface network rate history (name → history).
    pub net_if_history: Vec<(String, Vec<NetRateSample>)>,
    /// Round-trip latency of the last exec probe (ms).
    pub latency_ms: u32,
    /// When the SSH connection was established.
    pub connected_at: Instant,
    /// When status was last refreshed.
    pub last_refresh: Instant,
    /// Top processes by CPU usage.
    pub top_procs: Vec<ProcessInfo>,
}

/// A top-process entry from `ps`.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub name: String,
    pub cpu_pct: f32,
    pub mem_str: String,
}

impl Default for ServerStatus {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            os: String::new(),
            kernel: String::new(),
            hostname: String::new(),
            uptime: String::new(),
            load_avg: String::new(),
            mem_total_mb: 0,
            mem_used_mb: 0,
            cpu_usage_pct: 0.0,
            disks: Vec::new(),
            disk_usage: String::new(),
            net_ifs: Vec::new(),
            net_history: Vec::new(),
            net_if_history: Vec::new(),
            latency_ms: 0,
            connected_at: now,
            last_refresh: now,
            top_procs: Vec::new(),
        }
    }
}

impl ServerStatus {
    /// Human-readable connection duration.
    pub fn connection_duration(&self) -> String {
        let secs = self.connected_at.elapsed().as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m{}s", secs / 60, secs % 60)
        } else {
            format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

/// Handle to an active SSH session, usable from the sync UI thread.
pub struct SshSession {
    write_tx: tokio_mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: tokio_mpsc::UnboundedSender<(u16, u16)>,
    sftp_cmd_tx: tokio_mpsc::UnboundedSender<SftpCommand>,
    /// Shared server status, updated periodically by the background task.
    pub status: Arc<Mutex<ServerStatus>>,
    /// Receive SFTP results (directory listings, errors).
    pub sftp_result_rx: std_mpsc::Receiver<SftpResult>,
}

impl SshSession {
    /// Write data to the remote shell.
    pub fn write(&self, data: &[u8]) {
        let _ = self.write_tx.send(data.to_vec());
    }

    /// Resize the remote PTY.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.resize_tx.send((cols, rows));
    }

    /// Send an SFTP command (list dir, etc.).
    pub fn sftp_command(&self, cmd: SftpCommand) {
        let _ = self.sftp_cmd_tx.send(cmd);
    }
}

/// Spawn an SSH pane session on the given tokio runtime.
/// Returns `(output_rx, SshSession)` — output_rx receives remote PTY data.
pub fn spawn_ssh_pane(
    rt: &Runtime,
    profile: SshProfile,
    cols: u16,
    rows: u16,
) -> (std_mpsc::Receiver<Vec<u8>>, SshSession) {
    let (output_tx, output_rx) = std_mpsc::channel::<Vec<u8>>();
    let (write_tx, write_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, resize_rx) = tokio_mpsc::unbounded_channel::<(u16, u16)>();
    let (sftp_cmd_tx, sftp_cmd_rx) = tokio_mpsc::unbounded_channel::<SftpCommand>();
    let (sftp_result_tx, sftp_result_rx) = std_mpsc::channel::<SftpResult>();
    let status = Arc::new(Mutex::new(ServerStatus::default()));

    rt.spawn(ssh_task(
        profile,
        cols,
        rows,
        output_tx,
        write_rx,
        resize_rx,
        Arc::clone(&status),
        sftp_cmd_rx,
        sftp_result_tx,
    ));

    let session = SshSession {
        write_tx,
        resize_tx,
        sftp_cmd_tx,
        status,
        sftp_result_rx,
    };
    (output_rx, session)
}

/// Status probe interval (seconds). Short for decent network rate graphing.
const STATUS_REFRESH_SECS: u64 = 5;

/// Max history samples for network rate graph.
const NET_HISTORY_MAX: usize = 60;

/// One-liner that works on Linux/macOS/FreeBSD and outputs parseable status.
/// Collects: OS, kernel, hostname, uptime, memory, CPU idle (from /proc/stat),
/// per-disk usage (df -hT, all real filesystems), and network counters (/proc/net/dev).
const STATUS_PROBE_CMD: &str = r#"echo "OS:$(uname -s)"; echo "KERNEL:$(uname -r)"; echo "HOST:$(hostname)"; echo "UPTIME:$(uptime)"; if [ -f /proc/meminfo ]; then MT=$(awk '/MemTotal/{print int($2/1024)}' /proc/meminfo); MA=$(awk '/MemAvailable/{print int($2/1024)}' /proc/meminfo); echo "MEM:${MT}:$((MT-MA))"; else echo "MEM:0:0"; fi; if [ -f /proc/stat ]; then head -1 /proc/stat | awk '{t=0;for(i=2;i<=NF;i++)t+=$i; printf "CPU:%d:%d\n",t,$5}'; fi; df -hT 2>/dev/null | awk 'NR>1 && $2!="tmpfs" && $2!="devtmpfs" && $2!="squashfs" && $2!="overlay" && $1!="tmpfs"{printf "DF:%s:%s:%s:%s:%s:%s\n",$7,$2,$3,$4,$5,$6}' || df -h 2>/dev/null | awk 'NR>1 && $1!="tmpfs" && $1!="devtmpfs"{printf "DF:%s::%s:%s:%s:%s\n",$6,$2,$3,$4,$5}'; if [ -f /proc/net/dev ]; then awk -F'[: ]+' 'NR>2 && $2!="lo"{printf "NET:%s:%s:%s\n",$2,$3,$11}' /proc/net/dev 2>/dev/null; fi; ps aux --sort=-%cpu 2>/dev/null | awk 'NR>1 && NR<=9{m=$6; if(m>=1048576){ms=sprintf("%.1fG",m/1048576)}else if(m>=1024){ms=sprintf("%.1fM",m/1024)}else{ms=m"K"}; printf "PROC:%s:%.1f:%s\n",$11,$3,ms}'; echo "DONE""#;

/// Parse the output of STATUS_PROBE_CMD into a ServerStatus.
fn parse_status_output(raw: &str, latency_ms: u32, connected_at: Instant) -> ServerStatus {
    let mut s = ServerStatus {
        connected_at,
        latency_ms,
        last_refresh: Instant::now(),
        ..Default::default()
    };
    for line in raw.lines() {
        if let Some(val) = line.strip_prefix("OS:") {
            s.os = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("KERNEL:") {
            s.kernel = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("HOST:") {
            s.hostname = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("UPTIME:") {
            let trimmed = val.trim().to_string();
            if let Some(load_pos) = trimmed.find("load average") {
                s.load_avg = trimmed[load_pos..].replace("load average: ", "").replace("load average:", "").trim().to_string();
            }
            if let Some(up_pos) = trimmed.find("up ") {
                let after_up = &trimmed[up_pos + 3..];
                let end = after_up.find(" user").or_else(|| after_up.find("load")).unwrap_or(after_up.len());
                let raw_up = after_up[..end].trim().trim_end_matches(',').trim();
                s.uptime = raw_up.to_string();
            } else {
                s.uptime = trimmed;
            }
        } else if let Some(val) = line.strip_prefix("MEM:") {
            let parts: Vec<&str> = val.split(':').collect();
            if parts.len() == 2 {
                s.mem_total_mb = parts[0].trim().parse().unwrap_or(0);
                s.mem_used_mb = parts[1].trim().parse().unwrap_or(0);
            }
        } else if let Some(val) = line.strip_prefix("CPU:") {
            // CPU:total_ticks:idle_ticks — we store raw for delta computation
            let parts: Vec<&str> = val.split(':').collect();
            if parts.len() == 2 {
                // Store raw ticks temporarily; the caller computes usage %.
                s.cpu_usage_pct = {
                    let total: f32 = parts[0].trim().parse().unwrap_or(0.0);
                    let idle: f32 = parts[1].trim().parse().unwrap_or(0.0);
                    if total > 0.0 { (1.0 - idle / total) * 100.0 } else { 0.0 }
                };
            }
        } else if let Some(val) = line.strip_prefix("DF:") {
            // DF:mount:fstype:total:used:avail:use%
            let parts: Vec<&str> = val.splitn(6, ':').collect();
            if parts.len() >= 5 {
                let (mount, fstype, total, used, avail, use_pct) = if parts.len() == 6 {
                    (parts[0], parts[1], parts[2], parts[3], parts[4], parts[5])
                } else {
                    // fallback without fstype
                    (parts[0], "", parts[1], parts[2], parts[3], parts[4])
                };
                let di = DiskInfo {
                    mount: mount.to_string(),
                    fstype: fstype.to_string(),
                    total: total.to_string(),
                    used: used.to_string(),
                    avail: avail.to_string(),
                    use_pct: use_pct.to_string(),
                };
                if di.mount == "/" {
                    s.disk_usage = di.use_pct.clone();
                }
                s.disks.push(di);
            }
        } else if let Some(val) = line.strip_prefix("NET:") {
            // NET:iface:rx_bytes:tx_bytes
            let parts: Vec<&str> = val.splitn(3, ':').collect();
            if parts.len() == 3 {
                s.net_ifs.push(NetIfSnapshot {
                    name: parts[0].to_string(),
                    rx_bytes: parts[1].trim().parse().unwrap_or(0),
                    tx_bytes: parts[2].trim().parse().unwrap_or(0),
                });
            }
        } else if let Some(val) = line.strip_prefix("PROC:") {
            // PROC:command_name:cpu_pct:mem_str
            let parts: Vec<&str> = val.splitn(3, ':').collect();
            if parts.len() == 3 {
                let name = parts[0].rsplit('/').next().unwrap_or(parts[0]).to_string();
                let cpu_pct: f32 = parts[1].trim().parse().unwrap_or(0.0);
                let mem_str = parts[2].trim().to_string();
                s.top_procs.push(ProcessInfo { name, cpu_pct, mem_str });
            }
        } else if line.starts_with("DISK:") {
            // Legacy fallback
            if let Some(val) = line.strip_prefix("DISK:") {
                s.disk_usage = val.trim().to_string();
            }
        }
    }
    s
}

/// Compute per-interface and aggregate rate samples from consecutive snapshots.
fn compute_net_rates(
    prev: &[NetIfSnapshot],
    curr: &[NetIfSnapshot],
    interval_secs: f64,
) -> (NetRateSample, Vec<(String, NetRateSample)>) {
    let mut total_rx: u64 = 0;
    let mut total_tx: u64 = 0;
    let mut per_if = Vec::new();
    if interval_secs <= 0.0 {
        return (NetRateSample::default(), per_if);
    }
    for c in curr {
        if let Some(p) = prev.iter().find(|p| p.name == c.name) {
            let drx = c.rx_bytes.saturating_sub(p.rx_bytes);
            let dtx = c.tx_bytes.saturating_sub(p.tx_bytes);
            total_rx += drx;
            total_tx += dtx;
            per_if.push((c.name.clone(), NetRateSample {
                rx_bps: drx as f64 / interval_secs,
                tx_bps: dtx as f64 / interval_secs,
            }));
        }
    }
    (
        NetRateSample {
            rx_bps: total_rx as f64 / interval_secs,
            tx_bps: total_tx as f64 / interval_secs,
        },
        per_if,
    )
}

async fn probe_server_status(
    conn: &SshConnection,
    connected_at: Instant,
    prev_status: &ServerStatus,
) -> Option<ServerStatus> {
    // Measure true round-trip latency with a lightweight echo
    let ping_start = Instant::now();
    let ping_ok = conn.exec_command("echo ok").await.is_ok();
    let latency_ms = if ping_ok { ping_start.elapsed().as_millis() as u32 } else { 0 };

    match conn.exec_command(STATUS_PROBE_CMD).await {
        Ok(output) => {
            let mut st = parse_status_output(&output, latency_ms, connected_at);

            // Compute network rates from previous snapshot
            let interval = prev_status.last_refresh.elapsed().as_secs_f64();
            if !prev_status.net_ifs.is_empty() && interval > 0.5 {
                let (agg, per_if) = compute_net_rates(&prev_status.net_ifs, &st.net_ifs, interval);

                // Aggregate history
                st.net_history = prev_status.net_history.clone();
                st.net_history.push(agg);
                if st.net_history.len() > NET_HISTORY_MAX {
                    st.net_history.remove(0);
                }

                // Per-interface history
                st.net_if_history = prev_status.net_if_history.clone();
                for (name, sample) in per_if {
                    if let Some(entry) = st.net_if_history.iter_mut().find(|(n, _)| n == &name) {
                        entry.1.push(sample);
                        if entry.1.len() > NET_HISTORY_MAX {
                            entry.1.remove(0);
                        }
                    } else {
                        st.net_if_history.push((name, vec![sample]));
                    }
                }
            } else {
                st.net_history = prev_status.net_history.clone();
                st.net_if_history = prev_status.net_if_history.clone();
            }

            Some(st)
        }
        Err(e) => {
            warn!("server status probe failed: {e}");
            None
        }
    }
}

/// List a remote directory via SFTP and return an SftpResult.
async fn sftp_list_dir_impl(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> SftpResult {
    match sftp.read_dir(path).await {
        Ok(read_dir) => {
            let mut entries: Vec<RemoteEntry> = Vec::new();
            for entry in read_dir {
                let name: String = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let meta: russh_sftp::protocol::FileAttributes = entry.metadata();
                let perms = meta.permissions.unwrap_or(0);
                let is_dir = perms & 0o40000 != 0;  // S_IFDIR
                let is_symlink = perms & 0o120000 == 0o120000; // S_IFLNK
                let file_type = if is_symlink {
                    "link".to_string()
                } else if is_dir {
                    "dir".to_string()
                } else {
                    "file".to_string()
                };
                let full_path = if path == "/" {
                    format!("/{name}")
                } else {
                    format!("{path}/{name}")
                };
                entries.push(RemoteEntry {
                    name,
                    path: full_path,
                    is_dir,
                    is_symlink,
                    size: meta.size.unwrap_or(0),
                    permissions: meta.permissions.unwrap_or(0),
                    modified: meta.mtime.unwrap_or(0) as u64,
                    file_type,
                    owner: meta.user.clone().unwrap_or_else(|| {
                        meta.uid.map(|u| u.to_string()).unwrap_or_default()
                    }),
                    group: meta.group.clone().unwrap_or_else(|| {
                        meta.gid.map(|g| g.to_string()).unwrap_or_default()
                    }),
                });
            }
            // Sort: directories first, then alphabetically
            entries.sort_by(|a, b| {
                b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            SftpResult::DirListing {
                path: path.to_string(),
                entries,
            }
        }
        Err(e) => SftpResult::Error(format!("read_dir {path}: {e}")),
    }
}

/// Chunk size for SFTP transfers (32 KiB — balances throughput vs. progress granularity).
const TRANSFER_CHUNK: usize = 32 * 1024;

/// Download a remote file via SFTP to a local path, emitting progress events.
async fn sftp_download_impl(
    sftp: &russh_sftp::client::SftpSession,
    remote_path: &str,
    local_path: &str,
    progress_tx: &std_mpsc::Sender<SftpResult>,
) -> Result<(), String> {
    use tokio::io::AsyncReadExt;

    let mut remote_file = sftp.open(remote_path).await.map_err(|e| format!("open: {e}"))?;
    // Determine total size for progress reporting.
    let total = remote_file
        .metadata()
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);

    let mut local = std::fs::File::create(local_path).map_err(|e| format!("create local: {e}"))?;
    use std::io::Write;
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    let mut transferred: u64 = 0;
    // Initial 0% event so UI can register the job immediately.
    let _ = progress_tx.send(SftpResult::TransferProgress {
        dir: TransferDir::Download,
        key: remote_path.to_string(),
        bytes: 0,
        total,
    });
    loop {
        let n = remote_file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        local.write_all(&buf[..n]).map_err(|e| format!("write local: {e}"))?;
        transferred += n as u64;
        let _ = progress_tx.send(SftpResult::TransferProgress {
            dir: TransferDir::Download,
            key: remote_path.to_string(),
            bytes: transferred,
            total,
        });
    }
    info!("Downloaded {} → {} ({} bytes)", remote_path, local_path, transferred);
    Ok(())
}

/// Upload a local file to a remote path via SFTP, emitting progress events.
async fn sftp_upload_impl(
    sftp: &russh_sftp::client::SftpSession,
    local_path: &str,
    remote_path: &str,
    progress_tx: &std_mpsc::Sender<SftpResult>,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let total = std::fs::metadata(local_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let mut local = std::fs::File::open(local_path).map_err(|e| format!("open local: {e}"))?;
    let mut remote_file = sftp
        .create(remote_path)
        .await
        .map_err(|e| format!("create remote: {e}"))?;

    use std::io::Read;
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    let mut transferred: u64 = 0;
    let _ = progress_tx.send(SftpResult::TransferProgress {
        dir: TransferDir::Upload,
        key: remote_path.to_string(),
        bytes: 0,
        total,
    });
    loop {
        let n = local.read(&mut buf).map_err(|e| format!("read local: {e}"))?;
        if n == 0 {
            break;
        }
        remote_file
            .write_all(&buf[..n])
            .await
            .map_err(|e| format!("write remote: {e}"))?;
        transferred += n as u64;
        let _ = progress_tx.send(SftpResult::TransferProgress {
            dir: TransferDir::Upload,
            key: remote_path.to_string(),
            bytes: transferred,
            total,
        });
    }
    remote_file
        .flush()
        .await
        .map_err(|e| format!("flush remote: {e}"))?;
    info!("Uploaded {} → {} ({} bytes)", local_path, remote_path, transferred);
    Ok(())
}

async fn ssh_task(
    profile: SshProfile,
    cols: u16,
    rows: u16,
    output_tx: std_mpsc::Sender<Vec<u8>>,
    mut write_rx: tokio_mpsc::UnboundedReceiver<Vec<u8>>,
    mut resize_rx: tokio_mpsc::UnboundedReceiver<(u16, u16)>,
    status: Arc<Mutex<ServerStatus>>,
    mut sftp_cmd_rx: tokio_mpsc::UnboundedReceiver<SftpCommand>,
    sftp_result_tx: std_mpsc::Sender<SftpResult>,
) {
    let host = profile.host.clone();
    let user = profile.username.clone();

    // Connect
    let mut conn = match SshConnection::connect(profile).await {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("\x1b[31mSSH connection failed: {e}\x1b[0m\r\n");
            let _ = output_tx.send(msg.into_bytes());
            return;
        }
    };

    let connected_at = Instant::now();
    {
        let mut s = status.lock().unwrap();
        s.connected_at = connected_at;
    }

    info!(host = %host, user = %user, "SSH connected, opening shell");

    // Open shell channel
    let mut channel = match conn.open_shell(cols as u32, rows as u32).await {
        Ok(ch) => ch,
        Err(e) => {
            let msg = format!("\x1b[31mFailed to open remote shell: {e}\x1b[0m\r\n");
            let _ = output_tx.send(msg.into_bytes());
            return;
        }
    };

    info!(host = %host, "remote shell opened");

    // Inject shell integration via stdin. We send the setup commands followed
    // by a sentinel echo and `clear`. All PTY output (including the echoed
    // command text) is eaten until we see the sentinel. Only the `clear`
    // screen-reset that follows is forwarded, giving the user a clean start.
    {
        // Match with preceding newline so we skip the echoed command text
        // ("echo __NEXTERM_INIT_DONE__") and only match the actual output.
        const SENTINEL: &[u8] = b"\n__NEXTERM_INIT_DONE__";
        let inject = concat!(
            " if [ -n \"$BASH_VERSION\" ]; then ",
            "PROMPT_COMMAND='printf \"\\033]133;A\\007\"'\"${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"; ",
            "PS1=\"$PS1\"'\\[\\033]133;B\\007\\]'; ",
            "elif [ -n \"$ZSH_VERSION\" ]; then ",
            "precmd() { printf '\\033]133;A\\007'; }; ",
            "preexec() { printf '\\033]133;B\\007'; }; ",
            "fi; echo __NEXTERM_INIT_DONE__; clear\n"
        );
        if let Err(e) = channel.data(inject.as_bytes()).await {
            warn!(host = %host, "failed to inject shell integration: {e}");
        } else {
            info!(host = %host, "shell integration injected, filtering output until sentinel");
            // Eat all output until we see the sentinel.
            let mut buf = Vec::new();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                tokio::select! {
                    msg = channel.wait() => {
                        match msg {
                            Some(ChannelMsg::Data { ref data }) => {
                                buf.extend_from_slice(data);
                                if let Some(pos) = buf.windows(SENTINEL.len()).position(|w| w == SENTINEL) {
                                    // Skip sentinel + trailing newlines
                                    let after = pos + SENTINEL.len();
                                    let start = buf[after..].iter()
                                        .position(|&b| b != b'\r' && b != b'\n')
                                        .map(|p| after + p)
                                        .unwrap_or(buf.len());
                                    // Forward the rest (clear screen output + prompt)
                                    if start < buf.len() {
                                        let _ = output_tx.send(buf[start..].to_vec());
                                    }
                                    break;
                                }
                            }
                            Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                                buf.extend_from_slice(data);
                            }
                            Some(ChannelMsg::Eof) | None => break,
                            _ => {}
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        warn!(host = %host, "sentinel timeout, forwarding buffered output");
                        if !buf.is_empty() {
                            let _ = output_tx.send(buf);
                        }
                        break;
                    }
                }
            }
            info!(host = %host, "shell integration active");
        }
    }

    // Initial server status probe
    {
        let prev = status.lock().unwrap().clone();
        if let Some(st) = probe_server_status(&conn, connected_at, &prev).await {
            info!(
                host = %host,
                os = %st.os,
                hostname = %st.hostname,
                latency_ms = st.latency_ms,
                "server status probed"
            );
            *status.lock().unwrap() = st;
        }
    }

    // Open SFTP session (lazy, on a separate channel)
    let sftp_session = match conn.open_sftp().await {
        Ok(s) => {
            info!(host = %host, "SFTP session opened");
            Some(s)
        }
        Err(e) => {
            warn!(host = %host, "SFTP open failed (file browser unavailable): {e}");
            None
        }
    };

    // Track current SFTP directory for refresh after mkdir/touch
    let mut sftp_current_dir = String::from("/");

    // Auto-list home directory if SFTP is available
    if let Some(ref sftp) = sftp_session {
        if let Ok(home_path) = sftp.canonicalize(".").await {
            sftp_current_dir = home_path.clone();
            let entries = sftp_list_dir_impl(sftp, &home_path).await;
            let _ = sftp_result_tx.send(entries);
        }
    }

    // Refresh timer
    let mut refresh_interval =
        tokio::time::interval(std::time::Duration::from_secs(STATUS_REFRESH_SECS));
    refresh_interval.tick().await; // consume the immediate first tick

    // Main I/O loop
    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { ref data }) => {
                        if output_tx.send(data.to_vec()).is_err() {
                            break; // pane dropped
                        }
                    }
                    Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                        if output_tx.send(data.to_vec()).is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        let msg = format!("\r\n\x1b[33m[Process exited with status {exit_status}]\x1b[0m\r\n");
                        let _ = output_tx.send(msg.into_bytes());
                        break;
                    }
                    Some(ChannelMsg::Eof) | None => {
                        let msg = b"\r\n\x1b[33m[Connection closed]\x1b[0m\r\n".to_vec();
                        let _ = output_tx.send(msg);
                        break;
                    }
                    _ => {}
                }
            }

            Some(data) = write_rx.recv() => {
                if let Err(e) = channel.data(&data[..]).await {
                    error!(host = %host, "SSH write error: {e}");
                    break;
                }
            }

            Some((c, r)) = resize_rx.recv() => {
                let _ = channel.window_change(c as u32, r as u32, 0, 0).await;
            }

            Some(cmd) = sftp_cmd_rx.recv() => {
                if let Some(ref sftp) = sftp_session {
                    let result = match cmd {
                        SftpCommand::ListDir(path) => {
                            sftp_current_dir = path.clone();
                            sftp_list_dir_impl(sftp, &path).await
                        }
                        SftpCommand::GoHome => {
                            match sftp.canonicalize(".").await {
                                Ok(home) => {
                                    sftp_current_dir = home.clone();
                                    sftp_list_dir_impl(sftp, &home).await
                                }
                                Err(e) => SftpResult::Error(format!("canonicalize home: {e}")),
                            }
                        }
                        SftpCommand::Mkdir(path) => {
                            match sftp.create_dir(&path).await {
                                Ok(_) => {
                                    info!("SFTP mkdir: {}", path);
                                    sftp_list_dir_impl(sftp, &sftp_current_dir).await
                                }
                                Err(e) => SftpResult::Error(format!("mkdir {}: {e}", path)),
                            }
                        }
                        SftpCommand::Touch(path) => {
                            match sftp.create(&path).await {
                                Ok(_f) => {
                                    info!("SFTP touch: {}", path);
                                    sftp_list_dir_impl(sftp, &sftp_current_dir).await
                                }
                                Err(e) => SftpResult::Error(format!("touch {}: {e}", path)),
                            }
                        }
                        SftpCommand::Download { remote_path, local_path } => {
                            match sftp_download_impl(sftp, &remote_path, &local_path, &sftp_result_tx).await {
                                Ok(_) => SftpResult::DownloadDone { remote_path, local_path },
                                Err(e) => SftpResult::Error(format!("download {}: {e}", remote_path)),
                            }
                        }
                        SftpCommand::Upload { local_path, remote_path } => {
                            match sftp_upload_impl(sftp, &local_path, &remote_path, &sftp_result_tx).await {
                                Ok(_) => {
                                    // Refresh listing if upload landed in current dir
                                    if remote_path.rsplitn(2, '/').nth(1).unwrap_or("/") == sftp_current_dir
                                        || sftp_current_dir.is_empty()
                                    {
                                        let _ = sftp_result_tx.send(
                                            sftp_list_dir_impl(sftp, &sftp_current_dir).await,
                                        );
                                    }
                                    SftpResult::UploadDone { local_path, remote_path }
                                }
                                Err(e) => SftpResult::Error(format!("upload {}: {e}", remote_path)),
                            }
                        }
                    };
                    let _ = sftp_result_tx.send(result);
                } else {
                    let _ = sftp_result_tx.send(SftpResult::Error("SFTP not available".into()));
                }
            }

            _ = refresh_interval.tick() => {
                let prev = status.lock().unwrap().clone();
                if let Some(st) = probe_server_status(&conn, connected_at, &prev).await {
                    *status.lock().unwrap() = st;
                }
            }

            else => break,
        }
    }

    // Cleanup
    info!(host = %host, "SSH session ended");
    let _ = conn.disconnect().await;
}
