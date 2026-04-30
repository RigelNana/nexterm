//! SFTP remote file browser: listing, navigation, and search.

use crate::RemoteEntry;
use anyhow::Result;

/// Remote file browser state for a single SSH session.
pub struct RemoteBrowser {
    pub current_path: String,
    pub entries: Vec<RemoteEntry>,
}

impl RemoteBrowser {
    pub fn new() -> Self {
        Self {
            current_path: String::from("/"),
            entries: Vec::new(),
        }
    }

    /// Navigate to a directory (placeholder — actual SFTP calls go here).
    pub async fn navigate(&mut self, path: &str) -> Result<()> {
        self.current_path = path.to_string();
        // TODO: sftp.readdir(path) → populate self.entries
        self.entries.clear();
        Ok(())
    }

    /// Navigate up one level.
    pub async fn go_up(&mut self) -> Result<()> {
        if let Some(parent) = std::path::Path::new(&self.current_path).parent() {
            let parent = parent.to_string_lossy().to_string();
            self.navigate(&parent).await?;
        }
        Ok(())
    }
}
