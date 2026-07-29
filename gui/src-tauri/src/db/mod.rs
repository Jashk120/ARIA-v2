use rusqlite::{params, Connection, Result as SqlResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

// ── Chat message stored in DB ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub timestamp: i64,
    /// 'text' | 'daemon_task' | 'thought' | 'action' | 'observation' | 'final'
    /// | 'chat' | 'ask' | 'ask_self' | 'error' — see agent/mod.rs FrontendEvent
    /// and daemon.rs DaemonEvent for the full set. Defaults to 'text' for the
    /// plain user/assistant messages this table originally stored.
    #[serde(default = "default_event_type")]
    pub event_type: String,
    /// Structured extras a plain `content` string can't hold — e.g. an
    /// action's {skill, args}, or an ask's {kind}. Frontend re-parses this
    /// on load the same way it reads a live daemon_event's payload.
    #[serde(default)]
    pub payload_json: Option<String>,
    /// Ties every event belonging to one delegated daemon task together so
    /// a reload can rebuild the nested daemon_block the same way the live
    /// event stream built it. None for standalone messages (user text,
    /// plain chat replies, the router's own ask_self).
    #[serde(default)]
    pub group_id: Option<String>,
}

fn default_event_type() -> String {
    "text".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingConfirmation {
    pub task_id: String,
    pub content: String,
    pub kind: Option<String>,
    pub skill_type: String,
}

// ── Global DB connection (Mutex-protected single connection) ─────────────────

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (or create) the SQLite database at the given path and run migrations.
    pub fn open(path: PathBuf) -> SqlResult<Self> {
        let conn = Connection::open(path)?;
        let db = Database {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    // ── Schema Migrations ─────────────────────────────────────────────────────

    fn migrate(&self) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                id          TEXT PRIMARY KEY,
                title       TEXT NOT NULL DEFAULT 'New Chat',
                created_at  INTEGER NOT NULL,
                pending_confirmation TEXT
            );

            CREATE TABLE IF NOT EXISTS messages (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id  TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role        TEXT    NOT NULL,
                content     TEXT    NOT NULL,
                timestamp   INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            ",
        )?;
        if !column_exists(&conn, "sessions", "pending_confirmation")? {
            conn.execute(
                "ALTER TABLE sessions ADD COLUMN pending_confirmation TEXT",
                [],
            )?;
        }
        if !column_exists(&conn, "messages", "event_type")? {
            conn.execute(
                "ALTER TABLE messages ADD COLUMN event_type TEXT NOT NULL DEFAULT 'text'",
                [],
            )?;
        }
        if !column_exists(&conn, "messages", "payload_json")? {
            conn.execute("ALTER TABLE messages ADD COLUMN payload_json TEXT", [])?;
        }
        if !column_exists(&conn, "messages", "group_id")? {
            conn.execute("ALTER TABLE messages ADD COLUMN group_id TEXT", [])?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_messages_group ON messages(group_id)",
                [],
            )?;
        }
        Ok(())
    }

    // ── Session Operations ─────────────────────────────────────────────────────

    /// Create a new chat session and return its ID.
    pub fn create_session(&self, id: &str, title: &str) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        let now = unix_now();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (id, title, created_at) VALUES (?1, ?2, ?3)",
            params![id, title, now],
        )?;
        Ok(())
    }

    /// List all sessions ordered by newest first.
    pub fn list_sessions(&self) -> SqlResult<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, title, created_at FROM sessions ORDER BY created_at DESC")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<SqlResult<Vec<_>>>()?;
        Ok(rows)
    }

    /// Delete a session and all its messages.
    pub fn delete_session(&self, id: &str) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn save_pending_confirmation(
        &self,
        session_id: &str,
        task_id: &str,
        content: &str,
        kind: Option<&str>,
        skill_type: &str,
    ) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        let pending = PendingConfirmation {
            task_id: task_id.to_string(),
            content: content.to_string(),
            kind: kind.map(str::to_string),
            skill_type: skill_type.to_string(),
        };
        let pending_json = serde_json::to_string(&pending)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        conn.execute(
            "UPDATE sessions SET pending_confirmation = ?2 WHERE id = ?1",
            params![session_id, pending_json],
        )?;
        Ok(())
    }

    pub fn load_pending_confirmation(
        &self,
        session_id: &str,
    ) -> SqlResult<Option<PendingConfirmation>> {
        let conn = self.conn.lock().unwrap();
        let pending_json: Option<String> = conn.query_row(
            "SELECT pending_confirmation FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(pending_json
            .and_then(|json| serde_json::from_str::<PendingConfirmation>(&json).ok()))
    }

    /// Clear a session's pending confirmation, keyed directly by session_id
    /// (the caller already knows which session it's resuming — no need to
    /// scan every session's stored JSON looking for a task_id match).
    pub fn clear_pending_confirmation(&self, session_id: &str) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET pending_confirmation = NULL WHERE id = ?1",
            params![session_id],
        )?;

        Ok(())
    }

    pub fn clear_all_pending_confirmations(&self) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE sessions SET pending_confirmation = NULL", [])?;
        Ok(())
    }

    // ── Message Operations ────────────────────────────────────────────────────

    /// Persist any event the agent produces — plain text, a daemon
    /// thought/action/observation/final/chat, an ask (daemon or the GUI's
    /// own router), or an error. This is the fix for the GUI previously
    /// only saving plain user/assistant text and silently dropping every
    /// delegated-task event: everything now goes through this one path.
    pub fn save_event(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        event_type: &str,
        payload_json: Option<&str>,
        group_id: Option<&str>,
    ) -> SqlResult<i64> {
        let conn = self.conn.lock().unwrap();
        let now = unix_now();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, event_type, payload_json, group_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session_id, role, content, now, event_type, payload_json, group_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Back-compat wrapper for plain text messages (user input, plain
    /// non-delegated assistant replies). Equivalent to
    /// `save_event(.., event_type: "text", payload_json: None, group_id: None)`.
    pub fn save_message(&self, session_id: &str, role: &str, content: &str) -> SqlResult<i64> {
        self.save_event(session_id, role, content, "text", None, None)
    }

    /// Load all events for a session ordered by oldest first. Includes
    /// every event_type — the frontend groups by group_id to rebuild
    /// daemon_block cards the same way the live stream built them.
    pub fn load_messages(&self, session_id: &str) -> SqlResult<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, role, content, timestamp, event_type, payload_json, group_id
             FROM messages
             WHERE session_id = ?1
             ORDER BY timestamp ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    timestamp: row.get(4)?,
                    event_type: row.get(5)?,
                    payload_json: row.get(6)?,
                    group_id: row.get(7)?,
                })
            })?
            .collect::<SqlResult<Vec<_>>>()?;
        Ok(rows)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> SqlResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(rows.iter().any(|name| name == column))
}

