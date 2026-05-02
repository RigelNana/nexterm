//! # nexterm-history
//!
//! Command history storage with SQLite FTS5 for fuzzy full-text search.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

/// A single history record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: Uuid,
    pub command: String,
    pub output_summary: String,
    pub exit_code: i32,
    pub session_id: Option<Uuid>,
    pub host: Option<String>,
    pub cwd: Option<String>,
    pub timestamp: i64,
}

/// History database backed by SQLite with FTS5.
pub struct HistoryDb {
    conn: Connection,
}

impl HistoryDb {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS history (
                id              TEXT PRIMARY KEY,
                command         TEXT NOT NULL,
                output_summary  TEXT,
                exit_code       INTEGER,
                session_id      TEXT,
                host            TEXT,
                cwd             TEXT,
                timestamp       INTEGER NOT NULL
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS history_fts USING fts5(
                command,
                output_summary,
                host,
                cwd,
                content='history',
                content_rowid='rowid'
            );

            CREATE TRIGGER IF NOT EXISTS history_ai AFTER INSERT ON history BEGIN
                INSERT INTO history_fts(rowid, command, output_summary, host, cwd)
                VALUES (new.rowid, new.command, new.output_summary, new.host, new.cwd);
            END;
            ",
        )?;
        info!("history database schema initialized");
        Ok(())
    }

    /// Insert a new history entry.
    pub fn insert(&self, entry: &HistoryEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO history (id, command, output_summary, exit_code, session_id, host, cwd, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                entry.id.to_string(),
                entry.command,
                entry.output_summary,
                entry.exit_code,
                entry.session_id.map(|u| u.to_string()),
                entry.host,
                entry.cwd,
                entry.timestamp,
            ],
        )?;
        Ok(())
    }

    /// Fuzzy search history using FTS5.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT h.id, h.command, h.output_summary, h.exit_code, h.session_id, h.host, h.cwd, h.timestamp
             FROM history h
             JOIN history_fts f ON h.rowid = f.rowid
             WHERE history_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let entries = stmt
            .query_map(rusqlite::params![query, limit as i64], |row| {
                Ok(HistoryEntry {
                    id: row
                        .get::<_, String>(0)
                        .map(|s| Uuid::parse_str(&s).unwrap_or_default())
                        .unwrap_or_default(),
                    command: row.get(1)?,
                    output_summary: row.get(2)?,
                    exit_code: row.get(3)?,
                    session_id: row
                        .get::<_, Option<String>>(4)?
                        .map(|s| Uuid::parse_str(&s).unwrap_or_default()),
                    host: row.get(5)?,
                    cwd: row.get(6)?,
                    timestamp: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Get the most recent history entries ordered by timestamp descending.
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, command, output_summary, exit_code, session_id, host, cwd, timestamp
             FROM history
             ORDER BY timestamp DESC
             LIMIT ?1",
        )?;

        let entries = stmt
            .query_map(rusqlite::params![limit as i64], |row| {
                Ok(HistoryEntry {
                    id: row
                        .get::<_, String>(0)
                        .map(|s| Uuid::parse_str(&s).unwrap_or_default())
                        .unwrap_or_default(),
                    command: row.get(1)?,
                    output_summary: row.get(2)?,
                    exit_code: row.get(3)?,
                    session_id: row
                        .get::<_, Option<String>>(4)?
                        .map(|s| Uuid::parse_str(&s).unwrap_or_default()),
                    host: row.get(5)?,
                    cwd: row.get(6)?,
                    timestamp: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Count total history entries.
    pub fn count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
        Ok(count as usize)
    }
}
