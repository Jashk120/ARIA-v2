use sqlite::{Connection, State};
use std::fs;
use tracing::info;

static SCHEMA: &str = include_str!("../../db/db.sql");

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn new() -> anyhow::Result<Self> {
        let mut db_path = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
        db_path.push(".aria");
        fs::create_dir_all(&db_path)?;
        db_path.push("daemon.db");

        let conn = sqlite::open(&db_path)?;
        let db = Self { conn };
        if !db.has_table("identity")? {
            db.run_migration()?;
        }
        db.ensure_config_table()?;
        db.ensure_skills_table()?;
        Ok(db)
    }

    fn has_table(&self, name: &str) -> anyhow::Result<bool> {
        let mut statement = self.conn
            .prepare("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?")?;
        statement.bind((1, name))?;
        if let State::Row = statement.next()? {
            let count: i64 = statement.read(0)?;
            return Ok(count > 0);
        }
        Ok(false)
    }

    fn run_migration(&self) -> anyhow::Result<()> {
        self.conn.execute(SCHEMA)?;
        Ok(())
    }

    fn ensure_config_table(&self) -> anyhow::Result<()> {
        if !self.has_table("config")? {
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS config (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );"
            )?;
        }
        Ok(())
    }

    fn ensure_skills_table(&self) -> anyhow::Result<()> {
        if !self.has_table("skills")? {
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS skills (
                    name         TEXT PRIMARY KEY,
                    version      TEXT NOT NULL,
                    wasm_path    TEXT NOT NULL,
                    installed_at TEXT DEFAULT CURRENT_TIMESTAMP
                );"
            )?;
        }
        Ok(())
    }

    // ── Config ────────────────────────────────────────────────────────────────

    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut statement = self.conn.prepare("SELECT value FROM config WHERE key = ?")?;
        statement.bind((1, key))?;
        if let State::Row = statement.next()? {
            return Ok(Some(statement.read::<String, _>(0)?));
        }
        Ok(None)
    }

    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut statement = self.conn
            .prepare("INSERT OR REPLACE INTO config (key, value) VALUES (?, ?)")?;
        statement.bind((1, key))?;
        statement.bind((2, value))?;
        statement.next()?;
        Ok(())
    }

    // ── Skills ────────────────────────────────────────────────────────────────

    pub fn install_skill(&self, name: &str, version: &str, wasm_path: &str) -> anyhow::Result<()> {
        let mut statement = self.conn
            .prepare("INSERT OR REPLACE INTO skills (name, version, wasm_path) VALUES (?, ?, ?)")?;
        statement.bind((1, name))?;
        statement.bind((2, version))?;
        statement.bind((3, wasm_path))?;
        statement.next()?;
        Ok(())
    }

    pub fn get_skill(&self, name: &str) -> anyhow::Result<Option<String>> {
        let mut statement = self.conn
            .prepare("SELECT wasm_path FROM skills WHERE name = ?")?;
        statement.bind((1, name))?;
        if let State::Row = statement.next()? {
            return Ok(Some(statement.read::<String, _>(0)?));
        }
        Ok(None)
    }

    pub fn list_skills(&self) -> anyhow::Result<Vec<(String, String)>> {
        let mut statement = self.conn
            .prepare("SELECT name, version FROM skills ORDER BY name")?;
        let mut skills = Vec::new();
        while let State::Row = statement.next()? {
            skills.push((
                statement.read::<String, _>(0)?,
                statement.read::<String, _>(1)?,
            ));
        }
        Ok(skills)
    }

    pub fn remove_skill(&self, name: &str) -> anyhow::Result<()> {
        let mut statement = self.conn.prepare("DELETE FROM skills WHERE name = ?")?;
        statement.bind((1, name))?;
        statement.next()?;
        Ok(())
    }

    // ── Identity ──────────────────────────────────────────────────────────────

    pub fn ensure_stub_identity(&self) -> anyhow::Result<()> {
        let mut statement = self.conn.prepare("SELECT count(*) FROM identity")?;
        if let State::Row = statement.next()? {
            let count: i64 = statement.read(0)?;
            if count == 0 {
                info!("No identity found. Creating stub identity for Phase 1...");
                self.conn.execute(
                    "INSERT INTO identity (did, public_key, private_key, manifest_path)
                     VALUES ('did:aria:jayesh', 'stub_pub', 'stub_priv', '~/.aria/manifest.json')"
                )?;
            }
        }
        Ok(())
    }

    // ── Messages ──────────────────────────────────────────────────────────────

    pub fn save_message(&self, agent_did: &str, direction: &str, content: &str) -> anyhow::Result<()> {
        let mut statement = self.conn.prepare(
            "INSERT INTO messages (agent_did, direction, content) VALUES (?, ?, ?)"
        )?;
        statement.bind((1, agent_did))?;
        statement.bind((2, direction))?;
        statement.bind((3, content))?;
        statement.next()?;
        Ok(())
    }

    pub fn get_history(&self, agent_did: &str, limit: usize) -> anyhow::Result<Vec<(String, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT direction, content FROM messages WHERE agent_did = ? ORDER BY timestamp DESC LIMIT ?"
        )?;
        statement.bind((1, agent_did))?;
        statement.bind((2, limit as i64))?;

        let mut history = Vec::new();
        while let State::Row = statement.next()? {
            let direction: String = statement.read(0)?;
            let content: String = statement.read(1)?;
            history.push((direction, content));
        }
        Ok(history)
    }
}