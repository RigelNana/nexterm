//! File watcher for config hot-reload.

use anyhow::Result;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc;
use tracing::info;

/// Watch a config file for changes and send reload signals.
/// Watches the parent directory so it works even if the file doesn't exist yet.
pub fn watch_config(path: &Path) -> Result<mpsc::Receiver<()>> {
    let (tx, rx) = mpsc::channel();
    let config_file = path.to_path_buf();

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let watch_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let file_name = path.file_name().map(|n| n.to_os_string());

    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if event.kind.is_modify() || event.kind.is_create() {
                    // Only trigger for our config file
                    let matches = file_name.as_ref().map_or(true, |name| {
                        event
                            .paths
                            .iter()
                            .any(|p| p.file_name().map_or(false, |n| n == name.as_os_str()))
                    });
                    if matches {
                        info!(file = ?config_file, "config file changed, triggering reload");
                        let _ = tx.send(());
                    }
                }
            }
        },
        notify::Config::default(),
    )?;

    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;

    // Leak the watcher so it stays alive (in production, store it properly)
    std::mem::forget(watcher);

    Ok(rx)
}
