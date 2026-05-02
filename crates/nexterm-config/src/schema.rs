//! Configuration schema: strongly-typed representation of config.toml.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub general: GeneralConfig,
    pub appearance: AppearanceConfig,
    pub terminal: TerminalConfig,
    pub keybindings: KeybindingsConfig,
    pub ssh: SshConfig,
    pub sftp: SftpConfig,
    pub ai: AiConfig,
    /// Quick-connect SSH profiles.
    #[serde(default)]
    pub ssh_profiles: Vec<SshProfileConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            appearance: AppearanceConfig::default(),
            terminal: TerminalConfig::default(),
            keybindings: KeybindingsConfig::default(),
            ssh: SshConfig::default(),
            sftp: SftpConfig::default(),
            ai: AiConfig::default(),
            ssh_profiles: Vec::new(),
        }
    }
}

/// A quick-connect SSH profile defined in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshProfileConfig {
    pub name: String,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default = "default_ssh_user")]
    pub username: String,
    /// "password", "key", or "agent"
    #[serde(default = "default_ssh_auth")]
    pub auth: String,
    /// Password (only if auth = "password"). Use env var reference like "$MY_PASS".
    #[serde(default)]
    pub password: Option<String>,
    /// Path to private key (only if auth = "key").
    #[serde(default)]
    pub key_path: Option<String>,
    /// Passphrase for private key.
    #[serde(default)]
    pub key_passphrase: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}
fn default_ssh_user() -> String {
    "root".into()
}
fn default_ssh_auth() -> String {
    "agent".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub startup_mode: String,
    pub default_shell: String,
    pub working_directory: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            startup_mode: "last_session".into(),
            default_shell: "auto".into(),
            working_directory: "~".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub theme: String,
    pub font_family: String,
    pub font_size: f32,
    pub opacity: f32,
    pub blur: bool,
    pub cursor_style: String,
    pub cursor_blink: bool,
    /// Terminal content padding in pixels (distance from text to window edge).
    pub padding: f32,
    /// Optional background image path.
    #[serde(default)]
    pub background_image: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: "gruvbox-dark".into(),
            font_family: "Maple Mono NF CN".into(),
            font_size: 14.0,
            opacity: 0.95,
            blur: true,
            cursor_style: "beam".into(),
            cursor_blink: true,
            padding: 4.0,
            background_image: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    pub scrollback_lines: usize,
    pub block_mode: bool,
    pub copy_on_select: bool,
    pub ligatures: bool,
    pub sixel: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: 100_000,
            block_mode: true,
            copy_on_select: false,
            ligatures: true,
            sixel: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    pub split_horizontal: String,
    pub split_vertical: String,
    pub new_tab: String,
    pub search_history: String,
    pub toggle_ai: String,
    pub multi_exec: String,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            split_horizontal: "Ctrl+Shift+D".into(),
            split_vertical: "Ctrl+Shift+E".into(),
            new_tab: "Ctrl+Shift+T".into(),
            search_history: "Ctrl+R".into(),
            toggle_ai: "Ctrl+Shift+A".into(),
            multi_exec: "Ctrl+Shift+M".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub keepalive_interval: u32,
    pub keepalive_count_max: u32,
    pub auto_reconnect: bool,
    pub default_auth: String,
    pub config_sync: bool,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            keepalive_interval: 30,
            keepalive_count_max: 3,
            auto_reconnect: true,
            default_auth: "key".into(),
            config_sync: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SftpConfig {
    pub show_hidden: bool,
    pub confirm_overwrite: bool,
    pub verify_checksum: String,
    pub max_concurrent_transfers: usize,
}

impl Default for SftpConfig {
    fn default() -> Self {
        Self {
            show_hidden: false,
            confirm_overwrite: true,
            verify_checksum: "md5".into(),
            max_concurrent_transfers: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    pub ambient_agent: bool,
    pub auto_suggest: bool,
    pub max_context_tokens: usize,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            model: "deepseek-chat".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: String::new(),
            api_key_env: "NEXTERM_AI_KEY".into(),
            ambient_agent: true,
            auto_suggest: true,
            max_context_tokens: 32_000,
        }
    }
}
