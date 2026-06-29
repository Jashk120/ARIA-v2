use rusqlite::{params, Connection};
use std::fs;
use tracing::info;

use crate::crypto;

// ── Schema ────────────────────────────────────────────────────────────────────

static SCHEMA: &str = "
-- Core identity. One row per daemon instance.
CREATE TABLE IF NOT EXISTS identity (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    did             TEXT UNIQUE NOT NULL,
    public_key      TEXT NOT NULL,          -- multibase base58btc Ed25519 verifying key
    manifest_path   TEXT NOT NULL,
    vc              TEXT,                   -- NULL Phase 2, signed VC JSON Phase 3
    created_at      TEXT DEFAULT CURRENT_TIMESTAMP
);

-- One row per agentic task. Sealed on completion with the final audit chain hash.
CREATE TABLE IF NOT EXISTS tasks (
    task_id         TEXT PRIMARY KEY,       -- UUIDv4
    agent_did       TEXT NOT NULL,
    source          TEXT NOT NULL,          -- 'mcp' | 'cli' | 'webhook'
    prompt_hash     TEXT NOT NULL,          -- SHA-256 of the raw prompt (plaintext never stored)
    status          TEXT NOT NULL DEFAULT 'running', -- 'running' | 'done' | 'failed'
    step_count      INTEGER NOT NULL DEFAULT 0,
    final_hash      TEXT,                   -- chain_hash of last audit entry; NULL until sealed
    task_chain_prev TEXT,                   -- hash of previous tasks row (global task chain)
    task_chain_sig  TEXT,                   -- Ed25519 signature over task_chain_prev
    created_at      TEXT DEFAULT CURRENT_TIMESTAMP,
    sealed_at       TEXT
);

-- One row per skill invocation. Chained within task_id only.
CREATE TABLE IF NOT EXISTS audit_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT NOT NULL REFERENCES tasks(task_id),
    agent_did       TEXT NOT NULL,
    step            INTEGER NOT NULL,       -- 1-based step index within this task
    skill_called    TEXT NOT NULL,
    input_hash      TEXT NOT NULL,          -- SHA-256 of skill args JSON
    result_hash     TEXT NOT NULL,          -- SHA-256 of skill output JSON
    success         INTEGER NOT NULL DEFAULT 1,
    prev_hash       TEXT NOT NULL,          -- chain_hash of previous step (empty string = genesis)
    chain_hash      TEXT NOT NULL,          -- SHA-256(prev_hash|step|skill|input_hash|result_hash|timestamp)
    signature       TEXT NOT NULL,          -- Ed25519(signing_key, chain_hash)
    timestamp       TEXT DEFAULT CURRENT_TIMESTAMP
);

-- Installed WASM skills.
CREATE TABLE IF NOT EXISTS skills (
    name            TEXT PRIMARY KEY,       -- 'search.web'
    version         TEXT NOT NULL,
    wasm_path       TEXT NOT NULL,
    manifest        TEXT,
    installed_at    TEXT DEFAULT CURRENT_TIMESTAMP
);

-- Runtime config (API keys, provider settings).
CREATE TABLE IF NOT EXISTS config (
    key             TEXT PRIMARY KEY,
    value           TEXT NOT NULL
);

-- Phase 3: cached manifests from remote DIDs for verification.
CREATE TABLE IF NOT EXISTS cached_manifests (
    did             TEXT PRIMARY KEY,
    manifest        TEXT NOT NULL,
    verified        INTEGER DEFAULT 0,
    cached_at       TEXT DEFAULT CURRENT_TIMESTAMP,
    expires_at      TEXT
);
";

pub struct Db {
    conn: Connection,
}

// ── Task status ───────────────────────────────────────────────────────────────

pub enum TaskStatus {
    Running,
    Done,
    Failed,
}

impl TaskStatus {
    fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Done    => "done",
            TaskStatus::Failed  => "failed",
        }
    }
}

// ── Db impl ───────────────────────────────────────────────────────────────────

impl Db {
    pub fn new() -> anyhow::Result<Self> {
        let mut db_path =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
        db_path.push(".aria");
        fs::create_dir_all(&db_path)?;
        db_path.push("daemon.db");

        let conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let db = Self { conn };
        db.run_migration()?;
        Ok(db)
    }

    fn run_migration(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        Ok(())
    }

    // ── Identity ──────────────────────────────────────────────────────────────

    pub fn ensure_identity(&self, did: &str) -> anyhow::Result<()> {
        let count: i64 = self.conn.query_row("SELECT count(*) FROM identity", [], |row| row.get(0))?;
        if count > 0 {
            return Ok(());
        }
        info!("No identity found in DB — generating fresh Ed25519 identity for {}", did);
        let identity = crypto::generate_identity(did)?;
        self.conn.execute(
            "INSERT INTO identity (did, public_key, manifest_path) VALUES (?, ?, ?)",
            params![identity.did, identity.public_key_multibase, "~/.aria/manifest.toml"],
        )?;
        Ok(())
    }

    pub fn get_identity(&self) -> anyhow::Result<Option<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT did, public_key FROM identity LIMIT 1")?;
        let mut rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        if let Some(res) = rows.next() { return Ok(Some(res?)); }
        Ok(None)
    }

    // ── Tasks ─────────────────────────────────────────────────────────────────

    pub fn create_task(
        &self,
        agent_did: &str,
        source: &str,
        prompt: &str,
        task_chain_sig: &str,
    ) -> anyhow::Result<String> {
        let task_id = new_uuid();
        let prompt_hash = crypto::sha256_hex_str(prompt);
        let now = now_iso8601();
        let task_chain_prev = self.compute_task_chain_link_hash(&task_id, &prompt_hash, &now)?;

        self.conn.execute(
            "INSERT INTO tasks (task_id, agent_did, source, prompt_hash, status, task_chain_prev, task_chain_sig, created_at)
             VALUES (?, ?, ?, ?, 'running', ?, ?, ?)",
            params![task_id, agent_did, source, prompt_hash, task_chain_prev, task_chain_sig, now],
        )?;
        Ok(task_id)
    }

    pub fn seal_task(&self, task_id: &str, status: TaskStatus) -> anyhow::Result<()> {
        let final_hash = self.get_last_step_hash(task_id)?;
        let now = now_iso8601();
        self.conn.execute(
            "UPDATE tasks SET status=?, final_hash=?, sealed_at=?,
             step_count=(SELECT count(*) FROM audit_log WHERE task_id=?)
             WHERE task_id=?",
            params![status.as_str(), final_hash, now, task_id, task_id],
        )?;
        Ok(())
    }

    // ── Audit Log ─────────────────────────────────────────────────────────────

    pub fn log_task_step(
        &self,
        task_id: &str,
        agent_did: &str,
        skill_called: &str,
        input_json: &str,
        result_json: &str,
        success: bool,
        signature: &str,
    ) -> anyhow::Result<()> {
        let input_hash  = crypto::sha256_hex_str(input_json);
        let result_hash = crypto::sha256_hex_str(result_json);
        let timestamp   = now_iso8601();
        let step = self.next_step_index(task_id)?;
        let prev_hash = self.get_last_step_hash(task_id)?;
        let chain_hash = crypto::compute_chain_hash(&prev_hash, &step.to_string(), skill_called, &input_hash, &result_hash, &timestamp);

        self.conn.execute(
            "INSERT INTO audit_log (task_id, agent_did, step, skill_called, input_hash, result_hash, success, prev_hash, chain_hash, signature, timestamp)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![task_id, agent_did, step as i64, skill_called, input_hash, result_hash, success as i64, prev_hash, chain_hash, signature, timestamp],
        )?;
        Ok(())
    }

    pub fn verify_task_chain(&self, task_id: &str) -> anyhow::Result<usize> {
        let (_, pub_key) = self.get_identity()?.ok_or_else(|| anyhow::anyhow!("No identity"))?;
        let mut stmt = self.conn.prepare("SELECT step, skill_called, input_hash, result_hash, prev_hash, chain_hash, signature, timestamp FROM audit_log WHERE task_id = ? ORDER BY step ASC")?;
        let mut rows = stmt.query([task_id])?;
        let mut expected_prev = String::new();
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let _step: i64 = row.get(0)?;
            let prev_hash: String = row.get(4)?;
            let chain_hash: String = row.get(5)?;
            let signature: String = row.get(6)?;
            
            if prev_hash != expected_prev {
                anyhow::bail!("Chain broken: prev_hash mismatch");
            }

            crypto::verify_signature(&pub_key, chain_hash.as_bytes(), &signature)?;
            expected_prev = chain_hash;
            count += 1;
        }
        Ok(count)
    }

    // ── Config ────────────────────────────────────────────────────────────────

    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM config WHERE key = ?")?;
        let mut rows = stmt.query_map([key], |row| row.get(0))?;
        if let Some(res) = rows.next() { return Ok(Some(res?)); }
        Ok(None)
    }

    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO config (key, value) VALUES (?, ?)", params![key, value])?;
        Ok(())
    }

    // ── Skills ────────────────────────────────────────────────────────────────

    pub fn install_skill(&self, name: &str, version: &str, wasm_path: &str) -> anyhow::Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO skills (name, version, wasm_path) VALUES (?, ?, ?)", params![name, version, wasm_path])?;
        Ok(())
    }

    pub fn get_skill(&self, name: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT wasm_path FROM skills WHERE name = ?")?;
        let mut rows = stmt.query_map([name], |row| row.get(0))?;
        if let Some(res) = rows.next() { return Ok(Some(res?)); }
        Ok(None)
    }

    pub fn list_skills(&self) -> anyhow::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT name, version FROM skills ORDER BY name")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut res = Vec::new();
        for r in rows { res.push(r?); }
        Ok(res)
    }

    pub fn remove_skill(&self, name: &str) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM skills WHERE name = ?", [name])?;
        Ok(())
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    pub fn get_next_step_info(&self, task_id: &str) -> anyhow::Result<(usize, String, String)> {
        Ok((self.next_step_index(task_id)?, self.get_last_step_hash(task_id)?, now_iso8601()))
    }

    pub fn get_task_link_info(&self, task_id: &str, prompt: &str) -> anyhow::Result<(String, String)> {
        let now = now_iso8601();
        let prompt_hash = crypto::sha256_hex_str(prompt);
        Ok((self.compute_task_chain_link_hash(task_id, &prompt_hash, &now)?, now))
    }

    fn compute_task_chain_link_hash(&self, task_id: &str, prompt_hash: &str, created_at: &str) -> anyhow::Result<String> {
        let prev: String = self.conn.query_row("SELECT task_chain_prev FROM tasks ORDER BY created_at DESC LIMIT 1", [], |row| row.get(0)).unwrap_or_default();
        Ok(crypto::sha256_hex_str(&format!("{}|{}|{}|{}", prev, task_id, prompt_hash, created_at)))
    }

    fn next_step_index(&self, task_id: &str) -> anyhow::Result<usize> {
        let n: i64 = self.conn.query_row("SELECT count(*) FROM audit_log WHERE task_id = ?", [task_id], |row| row.get(0))?;
        Ok((n + 1) as usize)
    }

    fn get_last_step_hash(&self, task_id: &str) -> anyhow::Result<String> {
        let mut stmt = self.conn.prepare("SELECT chain_hash FROM audit_log WHERE task_id = ? ORDER BY step DESC LIMIT 1")?;
        let mut rows = stmt.query_map([task_id], |row| row.get(0))?;
        if let Some(res) = rows.next() { return Ok(res?); }
        Ok(String::new())
    }
}

fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let sec = secs % 60; let min = (secs / 60) % 60; let hour = (secs / 3600) % 24;
    let days = secs / 86400; let z = days + 719468; let era = z / 146097; let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400; let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153; let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hour, min, sec)
}

fn new_uuid() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; b[8] = (b[8] & 0x3f) | 0x80;
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", u32::from_be_bytes(b[0..4].try_into().unwrap()), u16::from_be_bytes(b[4..6].try_into().unwrap()), u16::from_be_bytes(b[6..8].try_into().unwrap()), u16::from_be_bytes(b[8..10].try_into().unwrap()), { let mut arr = [0u8; 8]; arr[2..].copy_from_slice(&b[10..16]); u64::from_be_bytes(arr) })
}
