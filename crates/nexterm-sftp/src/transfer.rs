//! File transfer engine: upload, download, resume, checksum verification.

use crate::{TransferDirection, TransferJob, TransferState};
use anyhow::Result;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Event emitted during a transfer for progress tracking.
#[derive(Debug, Clone)]
pub enum TransferEvent {
    Progress { job_id: Uuid, bytes_transferred: u64, total_bytes: u64 },
    Completed { job_id: Uuid },
    Failed { job_id: Uuid, error: String },
}

/// Manages a queue of file transfers.
pub struct TransferManager {
    pub queue: Vec<TransferJob>,
    pub max_concurrent: usize,
    event_tx: mpsc::Sender<TransferEvent>,
    event_rx: mpsc::Receiver<TransferEvent>,
}

impl TransferManager {
    pub fn new(max_concurrent: usize) -> Self {
        let (event_tx, event_rx) = mpsc::channel(256);
        Self {
            queue: Vec::new(),
            max_concurrent,
            event_tx,
            event_rx,
        }
    }

    /// Enqueue a new transfer job.
    pub fn enqueue(&mut self, direction: TransferDirection, local_path: String, remote_path: String) -> Uuid {
        let id = Uuid::new_v4();
        self.queue.push(TransferJob {
            id,
            direction,
            local_path,
            remote_path,
            state: TransferState::Queued,
        });
        id
    }

    /// Get the next transfer event (non-blocking).
    pub async fn next_event(&mut self) -> Option<TransferEvent> {
        self.event_rx.recv().await
    }

    /// Start processing queued transfers.
    pub async fn process(&mut self) -> Result<()> {
        // TODO: spawn concurrent transfer tasks using SFTP channels
        Ok(())
    }
}
