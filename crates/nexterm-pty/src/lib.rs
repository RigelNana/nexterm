//! # nexterm-pty
//!
//! Cross-platform PTY abstraction for spawning local shell processes.
//! Includes shell integration scripts (OSC 133) that are automatically
//! injected when a shell is spawned.

use anyhow::Result;
use portable_pty::{CommandBuilder, NativePtySystem, PtyPair, PtySize, PtySystem};
use std::io::{Read, Write};
use tracing::info;

// Embed shell integration scripts at compile time.
const SHELL_INTEGRATION_PWSH: &str = include_str!("../shell-integration/pwsh.ps1");
const SHELL_INTEGRATION_BASH: &str = include_str!("../shell-integration/bash.sh");
const SHELL_INTEGRATION_ZSH: &str = include_str!("../shell-integration/zsh.sh");
const SHELL_INTEGRATION_FISH: &str = include_str!("../shell-integration/fish.sh");

/// Configuration for spawning a local PTY.
pub struct PtyConfig {
    /// Shell executable. Use "auto" or None for auto-detect.
    /// Special value "wsl" launches wsl.exe.
    pub shell: Option<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
    /// Disable shell integration injection (for raw/non-interactive uses).
    pub no_shell_integration: bool,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            shell: None,
            cwd: None,
            cols: 120,
            rows: 40,
            env: Vec::new(),
            no_shell_integration: false,
        }
    }
}

/// A handle to a running local PTY process.
pub struct LocalPty {
    pair: PtyPair,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

/// Detected shell kind for integration injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Pwsh,
    Bash,
    Zsh,
    Fish,
    Unknown,
}

impl ShellKind {
    fn detect(shell_path: &str) -> Self {
        let lower = shell_path.to_ascii_lowercase();
        if lower.contains("pwsh") || lower.contains("powershell") {
            ShellKind::Pwsh
        } else if lower.ends_with("bash") || lower.ends_with("bash.exe") {
            ShellKind::Bash
        } else if lower.ends_with("zsh") || lower.ends_with("zsh.exe") {
            ShellKind::Zsh
        } else if lower.ends_with("fish") || lower.ends_with("fish.exe") {
            ShellKind::Fish
        } else {
            ShellKind::Unknown
        }
    }
}

/// Write a shell integration script to a temporary file and return the path.
fn write_integration_file(name: &str, content: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join("nexterm-shell-integration");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(name);
    match std::fs::write(&path, content) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("failed to write shell integration {name}: {e}");
            None
        }
    }
}

impl LocalPty {
    /// Spawn a new local shell in a PTY.
    pub fn spawn(config: PtyConfig) -> Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = resolve_shell(config.shell.as_deref());
        let kind = ShellKind::detect(&shell);
        let mut cmd = build_command_with_integration(&shell, kind, !config.no_shell_integration);

        if let Some(cwd) = &config.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        info!(shell = %shell, kind = ?kind, "spawning local PTY");
        let child = pair.slave.spawn_command(cmd)?;

        Ok(Self { pair, child })
    }

    /// Get a reader to receive output from the PTY.
    pub fn reader(&self) -> Result<Box<dyn Read + Send>> {
        Ok(self.pair.master.try_clone_reader()?)
    }

    /// Take a writer to send input to the PTY (can only be called once).
    pub fn writer(&self) -> Result<Box<dyn Write + Send>> {
        Ok(self.pair.master.take_writer()?)
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.pair.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    /// Check if the child process has exited.
    pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
}

/// Build a `CommandBuilder` that launches the shell with integration
/// scripts injected (when `inject` is true and the shell is recognized).
fn build_command_with_integration(shell: &str, kind: ShellKind, inject: bool) -> CommandBuilder {
    if !inject || kind == ShellKind::Unknown {
        return CommandBuilder::new(shell);
    }

    match kind {
        ShellKind::Pwsh => {
            // Minimal integration: only wraps prompt with 133;A and 133;B.
            // No PSConsoleHostReadLine wrapping to avoid double-prompt issues.
            if let Some(path) =
                write_integration_file("nexterm_integration.ps1", SHELL_INTEGRATION_PWSH)
            {
                let script = path.to_string_lossy().to_string();
                let mut cmd = CommandBuilder::new(shell);
                cmd.arg("-NoLogo");
                cmd.arg("-NoExit");
                cmd.arg("-Command");
                cmd.arg(format!(". '{}'", script));
                cmd
            } else {
                CommandBuilder::new(shell)
            }
        }
        ShellKind::Bash => {
            // Bash: create a wrapper rcfile that sources user's bashrc then ours.
            if let Some(int_path) =
                write_integration_file("nexterm_integration.bash", SHELL_INTEGRATION_BASH)
            {
                let wrapper = format!(
                    "[ -f ~/.bashrc ] && source ~/.bashrc\nsource '{}'\n",
                    int_path.to_string_lossy(),
                );
                if let Some(wrapper_path) = write_integration_file("nexterm_bashrc", &wrapper) {
                    let mut cmd = CommandBuilder::new(shell);
                    cmd.arg("--rcfile");
                    cmd.arg(wrapper_path.to_string_lossy().as_ref());
                    cmd
                } else {
                    CommandBuilder::new(shell)
                }
            } else {
                CommandBuilder::new(shell)
            }
        }
        ShellKind::Zsh => {
            // Zsh: create a temp ZDOTDIR with .zshrc that sources user's + ours.
            if let Some(int_path) =
                write_integration_file("nexterm_integration.zsh", SHELL_INTEGRATION_ZSH)
            {
                let zdotdir = std::env::temp_dir()
                    .join("nexterm-shell-integration")
                    .join("zdotdir");
                let _ = std::fs::create_dir_all(&zdotdir);
                let user_zdotdir = std::env::var("ZDOTDIR")
                    .or_else(|_| std::env::var("HOME"))
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_default();
                let wrapper = format!(
                    "[ -f '{user_zdotdir}/.zshenv' ] && source '{user_zdotdir}/.zshenv'\n\
                     [ -f '{user_zdotdir}/.zshrc' ] && source '{user_zdotdir}/.zshrc'\n\
                     source '{}'\n",
                    int_path.to_string_lossy(),
                );
                let _ = std::fs::write(zdotdir.join(".zshrc"), &wrapper);
                let mut cmd = CommandBuilder::new(shell);
                cmd.env("ZDOTDIR", zdotdir.to_string_lossy().as_ref());
                cmd
            } else {
                CommandBuilder::new(shell)
            }
        }
        ShellKind::Fish => {
            // Fish: use --init-command to source our integration.
            if let Some(int_path) =
                write_integration_file("nexterm_integration.fish", SHELL_INTEGRATION_FISH)
            {
                let mut cmd = CommandBuilder::new(shell);
                cmd.arg("--init-command");
                cmd.arg(format!("source '{}'", int_path.to_string_lossy()));
                cmd
            } else {
                CommandBuilder::new(shell)
            }
        }
        ShellKind::Unknown => CommandBuilder::new(shell),
    }
}

/// Resolve which shell to launch.
/// - `None` or `Some("auto")` → auto-detect
/// - `Some("wsl")` → wsl.exe
/// - anything else → use as-is
fn resolve_shell(requested: Option<&str>) -> String {
    match requested {
        Some("wsl") => {
            #[cfg(windows)]
            {
                "wsl.exe".to_string()
            }
            #[cfg(not(windows))]
            {
                "/bin/bash".to_string()
            }
        }
        Some(s) if !s.is_empty() && s != "auto" => s.to_string(),
        _ => auto_detect_shell(),
    }
}

/// Auto-detect the best available shell.
fn auto_detect_shell() -> String {
    #[cfg(windows)]
    {
        // Try pwsh (PowerShell 7+), fall back to powershell, then cmd
        for candidate in &["pwsh.exe", "powershell.exe", "cmd.exe"] {
            if which_exists(candidate) {
                return candidate.to_string();
            }
        }
        "cmd.exe".to_string()
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

#[cfg(windows)]
fn which_exists(name: &str) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("where")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
