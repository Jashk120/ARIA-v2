use std::fs;

use futures_util::stream::Skip;
use rusqlite::{
    Connection,
    params,
};
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
    status          TEXT NOT NULL DEFAULT 'running', -- 'running' | 'done' | 'failed'| 'awaiting_confirmation'
    step_count      INTEGER NOT NULL DEFAULT 0,
    final_hash      TEXT,                   -- chain_hash of last audit entry; NULL until sealed
    task_chain_prev TEXT,                   -- hash of previous tasks row (global task chain)
    task_chain_sig  TEXT,                   -- Ed25519 signature over task_chain_prev
    history_json        TEXT,       
     pending_action_json TEXT, 
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

-- Model tool capability cache
CREATE TABLE IF NOT EXISTS model_capabilities (
    model_string    TEXT PRIMARY KEY,
    capability      TEXT NOT NULL,
    updated_at      TEXT DEFAULT CURRENT_TIMESTAMP
);

-- Phase 3: cached manifests from remote DIDs for verification.
CREATE TABLE IF NOT EXISTS cached_manifests (
    did             TEXT PRIMARY KEY,
    manifest        TEXT NOT NULL,
    verified        INTEGER DEFAULT 0,
    cached_at       TEXT DEFAULT CURRENT_TIMESTAMP,
    expires_at      TEXT
);
-- x402 payments made by the agent (Hedera testnet/mainnet).
CREATE TABLE IF NOT EXISTS payments (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT REFERENCES tasks(task_id), -- NULL if triggered outside a task
    agent_did       TEXT NOT NULL,
    skill_called    TEXT NOT NULL,           
    recipient       TEXT NOT NULL,           -- Hedera AccountId
    amount_hbar     REAL NOT NULL,
    memo            TEXT,
    transaction_id  TEXT NOT NULL UNIQUE,
    hashscan_url    TEXT NOT NULL,
    status          TEXT NOT NULL,
    timestamp       TEXT DEFAULT CURRENT_TIMESTAMP
);

-- Allowlist for payment recipients
CREATE TABLE IF NOT EXISTS payment_allowlist (
    agent_did       TEXT NOT NULL,
    account         TEXT NOT NULL,
    PRIMARY KEY(agent_did, account)
);

-- Holds on rolling daily payment budget
CREATE TABLE IF NOT EXISTS payment_holds (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_did       TEXT NOT NULL,
    payment_key     TEXT UNIQUE NOT NULL,
    amount_hbar     REAL NOT NULL,
    timestamp       TEXT DEFAULT CURRENT_TIMESTAMP
)";

pub struct Db {
    conn: std::sync::Mutex<Connection>,
}

// ── Task status ───────────────────────────────────────────────────────────────

pub enum TaskStatus {
    Running,
    Done,
    Failed,
    AwaitingConfirmation,
}

impl TaskStatus {
    fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::AwaitingConfirmation => "awaiting_confirmation",
        }
    }
}

// ── Model Capabilities ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCapability {
    Native,
    PromptFallback,
    Unverified,
}
// near TaskStatus/ToolCapability
#[derive(Debug, Clone, serde::Serialize)]
pub struct PaymentRecord {
    pub recipient: String,
    pub amount_hbar: f64,
    pub memo: Option<String>,
    pub transaction_id: String,
    pub hashscan_url: String,
    pub status: String,
    pub timestamp: String,
}

#[derive(Debug, Clone)]
pub struct TaskSession {
    pub status: String,
    pub history_json: Option<String>,
    pub pending_action_json: Option<String>,
}

impl ToolCapability {
    fn as_str(&self) -> &'static str {
        match self {
            ToolCapability::Native => "native",
            ToolCapability::PromptFallback => "prompt_fallback",
            ToolCapability::Unverified => "unverified",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "native" => ToolCapability::Native,
            "prompt_fallback" => ToolCapability::PromptFallback,
            _ => ToolCapability::Unverified,
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

        let db = Self { conn: std::sync::Mutex::new(conn) };
        db.run_migration()?;
        Ok(db)
    }

    fn run_migration(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(SCHEMA)?;
        // Existing DBs created before session persistence was added won't have
        // these columns (CREATE TABLE IF NOT EXISTS doesn't retrofit them).
        // Ignore the error if the column already exists.
        let _ = conn.execute("ALTER TABLE tasks ADD COLUMN history_json TEXT", []);
        let _ = conn.execute("ALTER TABLE tasks ADD COLUMN pending_action_json TEXT", []);

        Ok(())
    }

    // ── Identity ──────────────────────────────────────────────────────────────

    pub fn ensure_identity(&self, did: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT count(*) FROM identity", [], |row| row.get(0))?;
        if count > 0 {
            return Ok(());
        }
        info!("No identity found in DB — generating fresh Ed25519 identity for {}", did);
        let identity = crypto::generate_identity(did)?;
        conn.execute(
            "INSERT INTO identity (did, public_key, manifest_path) VALUES (?, ?, ?)",
            params![identity.did, identity.public_key_multibase, "~/.aria/manifest.toml"],
        )?;
        Ok(())
    }

    pub fn get_identity(&self) -> anyhow::Result<Option<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT did, public_key FROM identity LIMIT 1")?;
        let mut rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        if let Some(res) = rows.next() {
            return Ok(Some(res?));
        }
        Ok(None)
    }

    /// Generate a real task ID up front, so callers can compute and sign the
    /// task-chain link hash against the *actual* ID before the task row is
    /// created. Passing this into `create_task` (instead of letting
    /// `create_task` mint its own ID internally) keeps the signed hash and
    /// the persisted `task_chain_prev` in sync.
    pub fn new_task_id(&self) -> String {
        new_uuid()
    }
    pub fn create_task(
        &self,
        task_id: &str,
        agent_did: &str,
        source: &str,
        prompt: &str,
        task_chain_prev: &str,
        task_chain_sig: &str,
        created_at: &str,
    ) -> anyhow::Result<String> {
        let prompt_hash = crypto::sha256_hex_str(prompt);

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, agent_did, source, prompt_hash, status, task_chain_prev, task_chain_sig, created_at)
             VALUES (?, ?, ?, ?, 'running', ?, ?, ?)",
           params![task_id, agent_did, source, prompt_hash, task_chain_prev, task_chain_sig, created_at],
       )?;
        Ok(task_id.to_string())
    }

    pub fn seal_task(&self, task_id: &str, status: TaskStatus) -> anyhow::Result<()> {
        let final_hash = self.get_last_step_hash(task_id)?;
        let now = now_iso8601();
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE tasks SET status=?, final_hash=?, sealed_at=?,
             step_count=(SELECT count(*) FROM audit_log WHERE task_id=?)
             WHERE task_id=? AND status != 'awaiting_confirmation'",
            params![status.as_str(), final_hash, now, task_id, task_id],
        )?;
        if changed == 0 {
            anyhow::bail!("task {} is awaiting confirmation and was not sealed", task_id);
        }
        Ok(())
    }
    // ── Resumable sessions (human-in-the-loop confirmation) ─────────────────────

    /// Pause a task awaiting a human yes/no reply — e.g. before executing a
    /// payment-capable skill. Persists the full react-loop history so a
    /// follow-up request carrying the same task_id can resume exactly here,
    /// instead of every TCP connection starting a brand-new, context-free task.
    pub fn save_awaiting_confirmation(
        &self,
        task_id: &str,
        history_json: &str,
        pending_action_json: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tasks SET status='awaiting_confirmation', history_json=?, pending_action_json=? WHERE task_id=?",
            params![history_json, pending_action_json, task_id],
        )?;
        Ok(())
    }

    /// Clear the pending action and drop back to 'running' once the human has
    /// replied (confirmed or denied) and the loop is continuing.
    pub fn clear_pending_action(&self, task_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tasks SET status='running', pending_action_json=NULL WHERE task_id=?",
            params![task_id],
        )?;
        Ok(())
    }

    /// Load a task's session state so a new TCP connection can resume it by
    /// task_id, instead of starting fresh. Returns None if the task doesn't
    /// exist.
    pub fn get_task_session(&self, task_id: &str) -> anyhow::Result<Option<TaskSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT status, history_json, pending_action_json FROM tasks WHERE task_id = ?",
        )?;
        let mut rows = stmt.query([task_id])?;
        if let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            let history_json: Option<String> = row.get(1)?;
            let mut pending_action_json: Option<String> = row.get(2)?;

            if let Some(ref json_str) = pending_action_json {
                if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if let Some(asked_at) = val.get("asked_at").and_then(|v| v.as_u64()) {
                        let now_secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let current_flag =
                            val.get("status_flag").and_then(|v| v.as_str()).unwrap_or("pending");
                        if now_secs.saturating_sub(asked_at) >= 300 && current_flag != "unresponsive"
                        {
                            if let Some(obj) = val.as_object_mut() {
                                obj.insert(
                                    "status_flag".to_string(),
                                    serde_json::json!("unresponsive"),
                                );
                            }
                            let updated_str = val.to_string();
                            let _ = conn.execute(
                                "UPDATE tasks SET pending_action_json = ? WHERE task_id = ?",
                                params![&updated_str, task_id],
                            );
                            pending_action_json = Some(updated_str);
                        }
                    }
                }
            }

            return Ok(Some(TaskSession { status, history_json, pending_action_json }));
        }
        Ok(None)
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
        let input_hash = crypto::sha256_hex_str(input_json);
        let result_hash = crypto::sha256_hex_str(result_json);
        let timestamp = now_iso8601();
        let step = self.next_step_index(task_id)?;
        let prev_hash = self.get_last_step_hash(task_id)?;
        let chain_hash = crypto::compute_chain_hash(
            &prev_hash,
            &step.to_string(),
            skill_called,
            &input_hash,
            &result_hash,
            &timestamp,
        );

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO audit_log (task_id, agent_did, step, skill_called, input_hash, result_hash, success, prev_hash, chain_hash, signature, timestamp)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![task_id, agent_did, step as i64, skill_called, input_hash, result_hash, success as i64, prev_hash, chain_hash, signature, timestamp],
        )?;
        Ok(())
    }

    pub fn verify_task_chain(&self, task_id: &str) -> anyhow::Result<usize> {
        let (_, pub_key) = self.get_identity()?.ok_or_else(|| anyhow::anyhow!("No identity"))?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT step, skill_called, input_hash, result_hash, prev_hash, chain_hash, signature, timestamp FROM audit_log WHERE task_id = ? ORDER BY step ASC")?;
        let mut rows = stmt.query([task_id])?;
        let mut expected_prev = String::new();
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let step: i64 = row.get(0)?;
            let skill_called: String = row.get(1)?;
            let input_hash: String = row.get(2)?;
            let result_hash: String = row.get(3)?;
            let prev_hash: String = row.get(4)?;
            let chain_hash: String = row.get(5)?;
            let signature: String = row.get(6)?;
            let timestamp: String = row.get(7)?;

            if prev_hash != expected_prev {
                anyhow::bail!("Chain broken: prev_hash mismatch at step {}", step);
            }
            let recomputed = crypto::compute_chain_hash(
                &prev_hash,
                &step.to_string(),
                &skill_called,
                &input_hash,
                &result_hash,
                &timestamp,
            );
            if recomputed != chain_hash {
                anyhow::bail!(
                    "Chain broken: chain_hash does not match row content at step {}",
                    step
                );
            }

            crypto::verify_signature(&pub_key, chain_hash.as_bytes(), &signature)?;
            expected_prev = chain_hash;
            count += 1;
        }
        Ok(count)
    }

    // ── Config ────────────────────────────────────────────────────────────────

    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM config WHERE key = ?")?;
        let mut rows = stmt.query_map([key], |row| row.get(0))?;
        if let Some(res) = rows.next() {
            return Ok(Some(res?));
        }
        Ok(None)
    }

    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO config (key, value) VALUES (?, ?)",
            params![key, value],
        )?;
        Ok(())
    }

    // ── Capabilities ──────────────────────────────────────────────────────────

    pub fn get_model_capability(&self, model: &str) -> anyhow::Result<ToolCapability> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT capability FROM model_capabilities WHERE model_string = ?")?;
        let mut rows = stmt.query_map([model], |row| row.get(0))?;
        if let Some(res) = rows.next() {
            let cap_str: String = res?;
            return Ok(ToolCapability::from_str(&cap_str));
        }
        Ok(ToolCapability::Unverified)
    }

    pub fn set_model_capability(
        &self,
        model: &str,
        capability: ToolCapability,
    ) -> anyhow::Result<()> {
        let now = now_iso8601();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO model_capabilities (model_string, capability, updated_at) VALUES (?, ?, ?)",
            params![model, capability.as_str(), now],
        )?;
        Ok(())
    }

    // ── Skills ────────────────────────────────────────────────────────────────

    pub fn install_skill(&self, name: &str, version: &str, wasm_path: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO skills (name, version, wasm_path) VALUES (?, ?, ?)",
            params![name, version, wasm_path],
        )?;
        Ok(())
    }

    pub fn get_skill(&self, name: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT wasm_path FROM skills WHERE name = ?")?;
        let mut rows = stmt.query_map([name], |row| row.get(0))?;
        if let Some(res) = rows.next() {
            return Ok(Some(res?));
        }
        Ok(None)
    }

    pub fn list_skills(&self) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT name, version FROM skills ORDER BY name")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut res = Vec::new();
        for r in rows {
            res.push(r?);
        }
        Ok(res)
    }

    pub fn remove_skill(&self, name: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM skills WHERE name = ?", [name])?;
        Ok(())
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    pub fn get_next_step_info(&self, task_id: &str) -> anyhow::Result<(usize, String, String)> {
        Ok((self.next_step_index(task_id)?, self.get_last_step_hash(task_id)?, now_iso8601()))
    }

    pub fn get_task_link_info(
        &self,
        task_id: &str,
        prompt: &str,
    ) -> anyhow::Result<(String, String)> {
        let now = now_iso8601();
        let prompt_hash = crypto::sha256_hex_str(prompt);
        Ok((self.compute_task_chain_link_hash(task_id, &prompt_hash, &now)?, now))
    }

    fn compute_task_chain_link_hash(
        &self,
        task_id: &str,
        prompt_hash: &str,
        created_at: &str,
    ) -> anyhow::Result<String> {
        let conn = self.conn.lock().unwrap();
        let prev: String = conn
            .query_row(
                "SELECT task_chain_prev FROM tasks ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or_default();
        Ok(crypto::sha256_hex_str(&format!("{}|{}|{}|{}", prev, task_id, prompt_hash, created_at)))
    }

    fn next_step_index(&self, task_id: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 =
            conn.query_row("SELECT count(*) FROM audit_log WHERE task_id = ?", [task_id], |row| {
                row.get(0)
            })?;
        Ok((n + 1) as usize)
    }

    fn get_last_step_hash(&self, task_id: &str) -> anyhow::Result<String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT chain_hash FROM audit_log WHERE task_id = ? ORDER BY step DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([task_id], |row| row.get(0))?;
        if let Some(res) = rows.next() {
            return Ok(res?);
        }
        Ok(String::new())
    }

    //--------------Payment ------------------------------
    pub fn insert_payment(
        &self,
        task_id: Option<&str>,
        agent_did: &str,
        skill_called: &str,
        recipient: &str,
        amount_hbar: f64,
        memo: &str,
        transaction_id: &str,
        hashscan_url: &str,
        status: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
        "INSERT INTO payments (task_id, agent_did, skill_called, recipient, amount_hbar, memo, transaction_id, hashscan_url, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![task_id, agent_did, skill_called, recipient, amount_hbar, memo, transaction_id, hashscan_url, status],
    )?;
        Ok(())
    }

    pub fn update_payment_status(&self, transaction_id: &str, status: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE payments SET status = ?1 WHERE transaction_id = ?2",
            params![status, transaction_id],
        )?;
        Ok(())
    }

    /// Payments in the last `days` days, most recent first — used by query.payments skill.
    pub fn list_recent_payments(&self, days: i64) -> anyhow::Result<Vec<PaymentRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT recipient, amount_hbar, memo, transaction_id, hashscan_url, status, timestamp
         FROM payments
         WHERE timestamp >= datetime('now', ?1)
         ORDER BY timestamp DESC",
        )?;
        let days_modifier = format!("-{} days", days);
        let rows = stmt.query_map(params![days_modifier], |row| {
            Ok(PaymentRecord {
                recipient: row.get(0)?,
                amount_hbar: row.get(1)?,
                memo: row.get(2)?,
                transaction_id: row.get(3)?,
                hashscan_url: row.get(4)?,
                status: row.get(5)?,
                timestamp: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Payment Governance (Curb Port) ──────────────────────────────────────────

    pub fn is_account_allowlisted(&self, agent_did: &str, account: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM payment_allowlist WHERE agent_did = ? AND account = ?",
            params![agent_did, account],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn add_allowlist_entry(&self, agent_did: &str, account: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO payment_allowlist (agent_did, account) VALUES (?, ?)",
            params![agent_did, account],
        )?;
        Ok(())
    }

    pub fn remove_allowlist_entry(&self, agent_did: &str, account: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM payment_allowlist WHERE agent_did = ? AND account = ?",
            params![agent_did, account],
        )?;
        Ok(n > 0)
    }

    pub fn list_allowlist(&self, agent_did: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT account FROM payment_allowlist WHERE agent_did = ? ORDER BY account",
        )?;
        let rows = stmt.query_map([agent_did], |row| row.get(0))?;
        let mut res = Vec::new();
        for r in rows {
            res.push(r?);
        }
        Ok(res)
    }

    pub fn get_daily_committed_spend(&self, agent_did: &str) -> anyhow::Result<f64> {
        let conn = self.conn.lock().unwrap();
        let sum: f64 = conn.query_row(
            "SELECT COALESCE(SUM(amount_hbar), 0.0) FROM payments WHERE agent_did = ? AND status = 'SUCCESS' AND timestamp >= datetime('now', '-24 hours')",
            params![agent_did],
            |row| row.get(0),
        )?;
        Ok(sum)
    }

    pub fn get_daily_held_spend(&self, agent_did: &str) -> anyhow::Result<f64> {
        let conn = self.conn.lock().unwrap();
        let sum: f64 = conn.query_row(
            "SELECT COALESCE(SUM(amount_hbar), 0.0) FROM payment_holds WHERE agent_did = ?",
            params![agent_did],
            |row| row.get(0),
        )?;
        Ok(sum)
    }

    pub fn try_reserve_spend(
        &self,
        agent_did: &str,
        payment_key: &str,
        amount_hbar: f64,
        per_day_cap: Option<f64>,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        // Idempotent: same payment key re-checked returns true without creating a second hold
        let existing: i64 = conn.query_row(
            "SELECT count(*) FROM payment_holds WHERE agent_did = ? AND payment_key = ?",
            params![agent_did, payment_key],
            |row| row.get(0),
        )?;
        if existing > 0 {
            return Ok(true);
        }

        if let Some(cap) = per_day_cap {
            let committed: f64 = conn.query_row(
                "SELECT COALESCE(SUM(amount_hbar), 0.0) FROM payments WHERE agent_did = ? AND status = 'SUCCESS' AND timestamp >= datetime('now', '-24 hours')",
                params![agent_did],
                |row| row.get(0),
            )?;
            let held: f64 = conn.query_row(
                "SELECT COALESCE(SUM(amount_hbar), 0.0) FROM payment_holds WHERE agent_did = ?",
                params![agent_did],
                |row| row.get(0),
            )?;
            if committed + held + amount_hbar > cap {
                return Ok(false);
            }
        }

        let now = now_iso8601();
        conn.execute(
            "INSERT INTO payment_holds (agent_did, payment_key, amount_hbar, timestamp) VALUES (?, ?, ?, ?)",
            params![agent_did, payment_key, amount_hbar, now],
        )?;
        Ok(true)
    }

    pub fn release_spend_hold(&self, agent_did: &str, payment_key: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM payment_holds WHERE agent_did = ? AND payment_key = ?",
            params![agent_did, payment_key],
        )?;
        Ok(())
    }

    pub fn commit_spend_hold(&self, agent_did: &str, payment_key: &str) -> anyhow::Result<()> {
        self.release_spend_hold(agent_did, payment_key)
    }

    /// Called once on daemon startup. Removes holds that are leftover from a crash that
    /// occurred between a successful payment (which wrote to `payments`) and the hold
    /// release. Matches by agent_did + amount_hbar within a 48-hour window (generous
    /// enough to cover holds placed before a restart).
    /// Returns the number of stale holds deleted.
    pub fn reconcile_stale_holds(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();

        // Collect all current holds.
        let hold_rows: Vec<(i64, String, String, f64)> = {
            let mut stmt = conn.prepare(
                "SELECT id, agent_did, payment_key, amount_hbar FROM payment_holds",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, f64>(3)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut removed = 0usize;
        for (hold_id, agent_did, _payment_key, amount_hbar) in hold_rows {
            // Check if a SUCCESS payment with the same agent_did and amount exists
            // in the last 48 hours (covers holds that survived a daemon crash).
            let matched: i64 = conn.query_row(
                "SELECT count(*) FROM payments \
                 WHERE agent_did = ? \
                   AND ABS(amount_hbar - ?) < 0.000001 \
                   AND status = 'SUCCESS' \
                   AND timestamp >= datetime('now', '-48 hours')",
                rusqlite::params![agent_did, amount_hbar],
                |row| row.get(0),
            )?;

            if matched > 0 {
                conn.execute("DELETE FROM payment_holds WHERE id = ?", rusqlite::params![hold_id])?;
                removed += 1;
            }
        }

        Ok(removed)
    }
}

fn now_iso8601() -> String {
    use std::time::{
        SystemTime,
        UNIX_EPOCH,
    };
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let sec = secs % 60;
    let min = (secs / 60) % 60;
    let hour = (secs / 3600) % 24;
    let days = secs / 86400;
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hour, min, sec)
}

fn new_uuid() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(b[0..4].try_into().unwrap()),
        u16::from_be_bytes(b[4..6].try_into().unwrap()),
        u16::from_be_bytes(b[6..8].try_into().unwrap()),
        u16::from_be_bytes(b[8..10].try_into().unwrap()),
        {
            let mut arr = [0u8; 8];
            arr[2..].copy_from_slice(&b[10..16]);
            u64::from_be_bytes(arr)
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowlist_crud() -> anyhow::Result<()> {
        let temp_dir = std::env::temp_dir().join(format!("aria_db_test_{}", new_uuid()));
        std::fs::create_dir_all(&temp_dir)?;
        let db_path = temp_dir.join("test.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn: std::sync::Mutex::new(conn) };

        let agent = "did:aria:test";
        let acc1 = "0.0.1234";
        let acc2 = "0.0.5678";

        assert!(!db.is_account_allowlisted(agent, acc1)?);
        db.add_allowlist_entry(agent, acc1)?;
        assert!(db.is_account_allowlisted(agent, acc1)?);

        db.add_allowlist_entry(agent, acc2)?;
        let list = db.list_allowlist(agent)?;
        assert_eq!(list, vec![acc1.to_string(), acc2.to_string()]);

        let removed = db.remove_allowlist_entry(agent, acc1)?;
        assert!(removed);
        assert!(!db.is_account_allowlisted(agent, acc1)?);

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_spend_holds_and_cap() -> anyhow::Result<()> {
        let temp_dir = std::env::temp_dir().join(format!("aria_db_test_{}", new_uuid()));
        std::fs::create_dir_all(&temp_dir)?;
        let db_path = temp_dir.join("test.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn: std::sync::Mutex::new(conn) };

        let agent = "did:aria:test";
        let key1 = "key-001";
        let key2 = "key-002";

        // Reserve 5.0 with 10.0 cap -> should succeed
        let ok1 = db.try_reserve_spend(agent, key1, 5.0, Some(10.0))?;
        assert!(ok1);
        assert_eq!(db.get_daily_held_spend(agent)?, 5.0);

        // Idempotent re-check with same key -> should return true
        let ok1_idem = db.try_reserve_spend(agent, key1, 5.0, Some(10.0))?;
        assert!(ok1_idem);
        assert_eq!(db.get_daily_held_spend(agent)?, 5.0);

        // Try to reserve 6.0 with 10.0 cap (5 held + 6 new > 10) -> should fail
        let ok2 = db.try_reserve_spend(agent, key2, 6.0, Some(10.0))?;
        assert!(!ok2);

        // Release key1 hold
        db.release_spend_hold(agent, key1)?;
        assert_eq!(db.get_daily_held_spend(agent)?, 0.0);

        // Now reserving 6.0 succeeds
        let ok2_after = db.try_reserve_spend(agent, key2, 6.0, Some(10.0))?;
        assert!(ok2_after);

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    /// Reproduces the original bug: a failed payment must release the hold, not leave it stuck.
    ///
    /// Old (buggy) behaviour: commit_spend_hold was called BEFORE run_skill_raw.
    /// If run_skill_raw then failed, the hold was already deleted (committed), but no
    /// SUCCESS row existed in payments \u2014 so the budget looked consumed while the money
    /// never moved. In the REVERSE scenario (hold NOT committed on failure), the hold
    /// would stick around and permanently eat daily budget.
    ///
    /// New behaviour tested here:
    /// - reserve a hold
    /// - simulate a failed payment (no insert_payment, hold is explicitly released)
    /// - assert hold is gone (get_daily_held_spend == 0)
    /// - assert committed spend is still 0 (no SUCCESS row in payments)
    /// - assert the same budget can be reserved again immediately
    #[test]
    fn test_failed_payment_releases_hold_and_does_not_count_as_committed() -> anyhow::Result<()> {
        let temp_dir = std::env::temp_dir().join(format!("aria_db_test_{}", new_uuid()));
        std::fs::create_dir_all(&temp_dir)?;
        let db_path = temp_dir.join("test.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn: std::sync::Mutex::new(conn) };

        let agent = "did:aria:test";
        let pkey = "pay-key-fail-001";

        // Step 1: reserve a hold for 7.0 HBAR against a 10.0 cap.
        let reserved = db.try_reserve_spend(agent, pkey, 7.0, Some(10.0))?;
        assert!(reserved, "hold reservation should succeed");
        assert_eq!(db.get_daily_held_spend(agent)?, 7.0);
        assert_eq!(db.get_daily_committed_spend(agent)?, 0.0, "no committed spend yet");

        // Step 2: simulate the skill execution failing — release the hold explicitly
        // (this is what react_loop.rs now does in the is_error branch after run_skill_raw).
        db.release_spend_hold(agent, pkey)?;

        // Step 3: hold must be gone.
        assert_eq!(db.get_daily_held_spend(agent)?, 0.0, "hold must be released after failure");
        assert_eq!(db.get_daily_committed_spend(agent)?, 0.0, "no committed spend after failure");

        // Step 4: the same payment can be retried — full cap is available again.
        let retried = db.try_reserve_spend(agent, "pay-key-retry-001", 7.0, Some(10.0))?;
        assert!(retried, "budget must be fully available again after failed payment releases hold");

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    /// Verifies that reconcile_stale_holds cleans up holds that survived a daemon crash
    /// (i.e., the payment succeeded and wrote to `payments`, but the hold was never deleted).
    #[test]
    fn test_reconcile_stale_holds_removes_orphaned_holds() -> anyhow::Result<()> {
        let temp_dir = std::env::temp_dir().join(format!("aria_db_test_{}", new_uuid()));
        std::fs::create_dir_all(&temp_dir)?;
        let db_path = temp_dir.join("test.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn: std::sync::Mutex::new(conn) };

        let agent = "did:aria:crash";
        let pkey = "stale-hold-001";

        // Simulate: hold was reserved before the crash.
        db.try_reserve_spend(agent, pkey, 3.0, Some(20.0))?;
        assert_eq!(db.get_daily_held_spend(agent)?, 3.0);

        // Simulate: payment succeeded (insert_payment ran inside WASM before crash).
        db.insert_payment(
            None,
            agent,
            "transfer.pay",
            "0.0.9999",
            3.0,
            "memo",
            "0.0.1234@1234567890.123456789",
            "https://hashscan.io/testnet/tx/test",
            "SUCCESS",
        )?;

        // At this point the hold is orphaned: a SUCCESS row exists but the hold was
        // never deleted (crash happened between run_skill_raw returning and hold release).
        assert_eq!(db.get_daily_held_spend(agent)?, 3.0, "orphaned hold still present before reconcile");

        // Reconcile should remove the stale hold.
        let cleaned = db.reconcile_stale_holds()?;
        assert_eq!(cleaned, 1, "exactly one stale hold should be cleaned up");
        assert_eq!(db.get_daily_held_spend(agent)?, 0.0, "hold gone after reconcile");

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}
