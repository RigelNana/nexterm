//! # nexterm-sync
//!
//! Optional cross-device synchronization for settings, history, and SSH profiles.
//! Future: E2E encrypted cloud sync or self-hosted server.

/// Sync backend trait.
#[async_trait::async_trait]
pub trait SyncBackend: Send + Sync {
    /// Push local changes to the remote.
    async fn push(&self, data: &[u8]) -> anyhow::Result<()>;
    /// Pull remote changes.
    async fn pull(&self) -> anyhow::Result<Vec<u8>>;
}

/// Placeholder local-only "sync" that does nothing.
pub struct NoopSync;

#[async_trait::async_trait]
impl SyncBackend for NoopSync {
    async fn push(&self, _data: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }
    async fn pull(&self) -> anyhow::Result<Vec<u8>> {
        Ok(Vec::new())
    }
}
