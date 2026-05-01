//! SQLite-backed storage for SSH profiles and session groups.

use anyhow::Result;
use nexterm_ssh::{AuthMethod, SshProfile};
use rusqlite::{params, Connection};
use tracing::info;
use uuid::Uuid;

/// Persistent store for sessions and groups.
pub struct SessionStore {
    conn: Connection,
}

impl SessionStore {
    /// Open (or create) the session database at the given path.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS session_groups (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                parent_id   TEXT,
                sort_order  INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS ssh_profiles (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                host        TEXT NOT NULL,
                port        INTEGER DEFAULT 22,
                username    TEXT NOT NULL,
                auth_json   TEXT NOT NULL,
                proxy_jump  TEXT,
                env_json    TEXT,
                keepalive   INTEGER DEFAULT 30,
                tags_json   TEXT,
                group_id    TEXT,
                sort_order  INTEGER DEFAULT 0
            );
            ",
        )?;
        // Migrate: if the old table had FK constraints, recreate without them.
        // This is safe because SQLite CREATE TABLE IF NOT EXISTS won't touch existing tables,
        // but we check and fix FK issues by attempting a dummy insert/rollback.
        self.migrate_remove_fk()?;
        info!("session store schema initialized");
        Ok(())
    }

    /// Migrate old tables that had FOREIGN KEY constraints to new schema without FK.
    /// SQLite doesn't support ALTER TABLE DROP CONSTRAINT, so we recreate the table.
    fn migrate_remove_fk(&self) -> Result<()> {
        // Check if the existing table has FK constraints by inspecting the schema SQL
        let sql: Option<String> = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='ssh_profiles'",
            [],
            |row| row.get(0),
        ).ok();
        if let Some(sql) = sql {
            if sql.contains("FOREIGN KEY") {
                info!("migrating ssh_profiles: removing FK constraints");
                self.conn.execute_batch(
                    "
                    ALTER TABLE ssh_profiles RENAME TO _ssh_profiles_old;
                    CREATE TABLE ssh_profiles (
                        id          TEXT PRIMARY KEY,
                        name        TEXT NOT NULL,
                        host        TEXT NOT NULL,
                        port        INTEGER DEFAULT 22,
                        username    TEXT NOT NULL,
                        auth_json   TEXT NOT NULL,
                        proxy_jump  TEXT,
                        env_json    TEXT,
                        keepalive   INTEGER DEFAULT 30,
                        tags_json   TEXT,
                        group_id    TEXT,
                        sort_order  INTEGER DEFAULT 0
                    );
                    INSERT INTO ssh_profiles SELECT * FROM _ssh_profiles_old;
                    DROP TABLE _ssh_profiles_old;
                    ",
                )?;
                info!("migration complete");
            }
        }
        Ok(())
    }

    // ---- SSH Profile CRUD ----

    /// Insert or update an SSH profile.
    pub fn save_profile(&self, profile: &SshProfile) -> Result<()> {
        let auth_json = serde_json::to_string(&profile.auth)?;
        let proxy_json = serde_json::to_string(&profile.proxy_jump)?;
        let env_json = serde_json::to_string(&profile.env)?;
        let tags_json = serde_json::to_string(&profile.tags)?;
        let group_id = profile.group.as_deref().unwrap_or("");

        self.conn.execute(
            "INSERT INTO ssh_profiles (id, name, host, port, username, auth_json, proxy_jump, env_json, keepalive, tags_json, group_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                name=excluded.name, host=excluded.host, port=excluded.port,
                username=excluded.username, auth_json=excluded.auth_json,
                proxy_jump=excluded.proxy_jump, env_json=excluded.env_json,
                keepalive=excluded.keepalive, tags_json=excluded.tags_json,
                group_id=excluded.group_id",
            params![
                profile.id.to_string(),
                profile.name,
                profile.host,
                profile.port,
                profile.username,
                auth_json,
                proxy_json,
                env_json,
                profile.keepalive_interval,
                tags_json,
                group_id,
            ],
        )?;
        info!(id = %profile.id, name = %profile.name, "profile saved");
        Ok(())
    }

    /// Load all SSH profiles from the database.
    pub fn load_profiles(&self) -> Result<Vec<SshProfile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, host, port, username, auth_json, proxy_jump, env_json, keepalive, tags_json, group_id
             FROM ssh_profiles ORDER BY group_id, sort_order, name",
        )?;
        let profiles = stmt.query_map([], |row| {
            let id_str: String = row.get(0)?;
            let auth_json: String = row.get(5)?;
            let proxy_json: String = row.get(6)?;
            let env_json: String = row.get(7)?;
            let tags_json: String = row.get(9)?;
            let group_str: String = row.get(10)?;

            Ok((id_str, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                row.get::<_, u16>(3)?, row.get::<_, String>(4)?,
                auth_json, proxy_json, env_json,
                row.get::<_, u32>(8)?, tags_json, group_str))
        })?;

        let mut result = Vec::new();
        for row in profiles {
            let (id_str, name, host, port, username, auth_json, proxy_json, env_json, keepalive, tags_json, group_str) = row?;
            let id = Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4());
            let auth: AuthMethod = serde_json::from_str(&auth_json).unwrap_or(AuthMethod::Agent);
            let proxy_jump: Vec<String> = serde_json::from_str(&proxy_json).unwrap_or_default();
            let env: Vec<(String, String)> = serde_json::from_str(&env_json).unwrap_or_default();
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            let group = if group_str.is_empty() { None } else { Some(group_str) };

            result.push(SshProfile {
                id, name, host, port, username, auth,
                proxy_jump, env, keepalive_interval: keepalive,
                tags, group,
            });
        }
        info!(count = result.len(), "profiles loaded from store");
        Ok(result)
    }

    /// Delete a profile by its UUID.
    pub fn delete_profile(&self, id: &Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM ssh_profiles WHERE id = ?1",
            params![id.to_string()],
        )?;
        info!(id = %id, "profile deleted");
        Ok(())
    }

    /// Count the stored profiles.
    pub fn profile_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM ssh_profiles", [], |row| row.get(0),
        )?;
        Ok(count as usize)
    }
}
