//! # nexterm-config
//!
//! TOML-based configuration with hot-reload support.

pub mod schema;
pub mod watcher;

use anyhow::Result;
use std::path::PathBuf;
use tracing::info;

/// Resolve the default configuration file path.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nexterm")
        .join("config.toml")
}

/// Load configuration from a TOML file.
pub fn load_config(path: &std::path::Path) -> Result<schema::AppConfig> {
    if !path.exists() {
        info!("config file not found, using defaults");
        return Ok(schema::AppConfig::default());
    }
    let content = std::fs::read_to_string(path)?;
    let config: schema::AppConfig = toml::from_str(&content)?;
    info!(path = %path.display(), "config loaded");
    Ok(config)
}

/// Save configuration to a TOML file.
/// Creates parent directories if they don't exist.
pub fn save_config(path: &std::path::Path, config: &schema::AppConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(config)?;
    std::fs::write(path, content)?;
    info!(path = %path.display(), "config saved");
    Ok(())
}
