use sqlite::{Connection, State};
use std::fs;
use tracing::info;

use crate::crypto;

// ── Schema ────────────────────────────────────────────────────────────────────
//
// tasks      — one row per agentic task received from a client (MCP, webhook, etc.)
// audit_log  — one row per skill invocation, chained within its task
// tasks_chain— lightweight global chain over the tasks table itself, so deleted
//              tasks are detectable even though per-task chains are isolated
// identity   — one row: this daemon's Ed25519 DID
// skills     — installed WASM skill binaries
// config     — key/value store for runtime config (api keys etc.)
// cached_manifests — Phase 3: remote DID manifest cache

static SCHEMA: &str = "
-- Core identity. One row per daemon instance.
CREATE TABLE IF NOT EXISTS identity (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    did             TEXT UNIQUE NOT NULL,
    public_key      TEXT NOT NULL,          -- multibase base58btc Ed25519 verifying key
    private_key     TEXT NOT NULL,          -- AES-256-GCM encrypted at rest
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

        let conn = sqlite::open(&db_path)?;
        // Enable WAL for concurrent reads during long tasks
        conn.execute("PRAGMA journal_mode=WAL;")?;
        conn.execute("PRAGMA foreign_keys=ON;")?;

        let db = Self { conn };
        db.run_migration()?;
        Ok(db)
    }

    fn run_migration(&self) -> anyhow::Result<()> {
        self.conn.execute(SCHEMA)?;
        Ok(())
    }

    fn has_table(&self, name: &str) -> anyhow::Result<bool> {
        let mut stmt = self
            .conn
            .prepare("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?")?;
        stmt.bind((1, name))?;
        if let State::Row = stmt.next()? {
            let count: i64 = stmt.read(0)?;
            return Ok(count > 0);
        }
        Ok(false)
    }

    // ── Identity ──────────────────────────────────────────────────────────────

    /// Generate and store a real Ed25519 identity on first run. No-op if one exists.
    pub fn ensure_identity(&self, did: &str) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare("SELECT count(*) FROM identity")?;
        if let State::Row = stmt.next()? {
            if stmt.read::<i64, _>(0)? > 0 {
                return Ok(());
            }
        }

        info!("No identity found — generating Ed25519 keypair for {}", did);
        let identity = crypto::generate_identity(did)?;

        let mut stmt = self.conn.prepare(
            "INSERT INTO identity (did, public_key, private_key, manifest_path)
             VALUES (?, ?, ?, ?)",
        )?;
        stmt.bind((1, identity.did.as_str()))?;
        stmt.bind((2, identity.public_key_multibase.as_str()))?;
        stmt.bind((3, identity.encrypted_private_key_hex.as_str()))?;
        stmt.bind((4, "~/.aria/manifest.toml"))?;
        stmt.next()?;

        info!("Identity created — DID: {}", identity.did);
        Ok(())
    }

    /// Returns (did, public_key_multibase, encrypted_private_key_hex).
    pub fn get_identity(&self) -> anyhow::Result<Option<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT did, public_key, private_key FROM identity LIMIT 1")?;
        if let State::Row = stmt.next()? {
            return Ok(Some((
                stmt.read(0)?,
                stmt.read(1)?,
                stmt.read(2)?,
            )));
        }
        Ok(None)
    }

    // ── Tasks ─────────────────────────────────────────────────────────────────

    /// Open a new task. Returns the task_id (UUIDv4).
    /// `prompt` — the raw prompt string; only its SHA-256 is stored.
    /// `source` — "mcp" | "cli" | "webhook"
    pub fn create_task(
        &self,
        agent_did: &str,
        source: &str,
        prompt: &str,
    ) -> anyhow::Result<String> {
        let task_id = new_uuid();
        let prompt_hash = crypto::sha256_hex_str(prompt);
        let now = now_iso8601();

        // Global task chain: hash over the previous task's chain entry
        let (task_chain_prev, task_chain_sig) =
            self.compute_task_chain_link(agent_did, &task_id, &prompt_hash, &now)?;

        let mut stmt = self.conn.prepare(
            "INSERT INTO tasks
               (task_id, agent_did, source, prompt_hash, status,
                task_chain_prev, task_chain_sig, created_at)
             VALUES (?, ?, ?, ?, 'running', ?, ?, ?)",
        )?;
        stmt.bind((1, task_id.as_str()))?;
        stmt.bind((2, agent_did))?;
        stmt.bind((3, source))?;
        stmt.bind((4, prompt_hash.as_str()))?;
        stmt.bind((5, task_chain_prev.as_str()))?;
        stmt.bind((6, task_chain_sig.as_str()))?;
        stmt.bind((7, now.as_str()))?;
        stmt.next()?;

        info!("Task created: {} (source={})", task_id, source);
        Ok(task_id)
    }

    /// Seal a task on completion. Stores final_hash (last audit chain_hash) and status.
    pub fn seal_task(
        &self,
        task_id: &str,
        status: TaskStatus,
    ) -> anyhow::Result<()> {
        // Get the chain_hash of the last audit step for this task
        let final_hash = self.get_last_step_hash(task_id)?;
        let now = now_iso8601();

        let mut stmt = self.conn.prepare(
            "UPDATE tasks SET status=?, final_hash=?, sealed_at=?,
             step_count=(SELECT count(*) FROM audit_log WHERE task_id=?)
             WHERE task_id=?",
        )?;
        stmt.bind((1, status.as_str()))?;
        stmt.bind((2, final_hash.as_str()))?;
        stmt.bind((3, now.as_str()))?;
        stmt.bind((4, task_id))?;
        stmt.bind((5, task_id))?;
        stmt.next()?;

        info!("Task sealed: {} → {}", task_id, status.as_str());
        Ok(())
    }

    /// Returns (task_id, source, status, step_count, created_at, sealed_at) for recent tasks.
    pub fn list_tasks(&self, limit: usize) -> anyhow::Result<Vec<(String, String, String, i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, source, status, step_count, created_at, COALESCE(sealed_at, '')
             FROM tasks ORDER BY created_at DESC LIMIT ?",
        )?;
        stmt.bind((1, limit as i64))?;
        let mut rows = Vec::new();
        while let State::Row = stmt.next()? {
            rows.push((
                stmt.read(0)?,
                stmt.read(1)?,
                stmt.read(2)?,
                stmt.read(3)?,
                stmt.read(4)?,
                stmt.read(5)?,
            ));
        }
        Ok(rows)
    }

    // ── Audit Log ─────────────────────────────────────────────────────────────

    /// Append a signed, hash-chained audit entry for a task step.
    /// Chain is scoped to task_id — each task has its own genesis.
    pub fn log_task_step(
        &self,
        task_id: &str,
        agent_did: &str,
        skill_called: &str,
        input_json: &str,
        result_json: &str,
        success: bool,
    ) -> anyhow::Result<()> {
        let input_hash  = crypto::sha256_hex_str(input_json);
        let result_hash = crypto::sha256_hex_str(result_json);
        let timestamp   = now_iso8601();

        // Step index within this task
        let step = self.next_step_index(task_id)?;

        // Per-task chain: prev_hash is the chain_hash of the previous step in THIS task
        let prev_hash = self.get_last_step_hash(task_id)?;

        let chain_hash = crypto::compute_chain_hash(
            &prev_hash,
            &step.to_string(),
            skill_called,
            &input_hash,
            &result_hash,
            &timestamp,
        );

        let signature = self.sign_for_agent(agent_did, &chain_hash)?;

        let mut stmt = self.conn.prepare(
            "INSERT INTO audit_log
               (task_id, agent_did, step, skill_called,
                input_hash, result_hash, success,
                prev_hash, chain_hash, signature, timestamp)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        stmt.bind((1,  task_id))?;
        stmt.bind((2,  agent_did))?;
        stmt.bind((3,  step as i64))?;
        stmt.bind((4,  skill_called))?;
        stmt.bind((5,  input_hash.as_str()))?;
        stmt.bind((6,  result_hash.as_str()))?;
        stmt.bind((7,  success as i64))?;
        stmt.bind((8,  prev_hash.as_str()))?;
        stmt.bind((9,  chain_hash.as_str()))?;
        stmt.bind((10, signature.as_str()))?;
        stmt.bind((11, timestamp.as_str()))?;
        stmt.next()?;

        Ok(())
    }

    /// Verify the audit chain for a single task.
    /// Returns Ok(step_count) or Err at the first broken link / bad signature.
    pub fn verify_task_chain(&self, task_id: &str) -> anyhow::Result<usize> {
        let pub_key = match self.get_identity()? {
            Some((_, pub_key, _)) => pub_key,
            None => anyhow::bail!("No identity found"),
        };

        // Also check the sealed final_hash matches the last audit row
        let (expected_final, db_status) = self.get_task_seal(task_id)?;

        let mut stmt = self.conn.prepare(
            "SELECT step, skill_called, input_hash, result_hash,
                    prev_hash, chain_hash, signature, timestamp
             FROM audit_log WHERE task_id = ? ORDER BY step ASC",
        )?;
        stmt.bind((1, task_id))?;

        let mut expected_prev = String::new();
        let mut count = 0usize;
        let mut last_chain_hash = String::new();

        while let State::Row = stmt.next()? {
            let step:        i64    = stmt.read(0)?;
            let skill:       String = stmt.read(1)?;
            let input_hash:  String = stmt.read(2)?;
            let result_hash: String = stmt.read(3)?;
            let prev_hash:   String = stmt.read(4)?;
            let chain_hash:  String = stmt.read(5)?;
            let signature:   String = stmt.read(6)?;
            let timestamp:   String = stmt.read(7)?;

            // 1. prev_hash must match previous step's chain_hash
            if prev_hash != expected_prev {
                anyhow::bail!(
                    "Chain broken at step {} — prev_hash mismatch", step
                );
            }

            // 2. Recompute chain_hash
            let recomputed = crypto::compute_chain_hash(
                &prev_hash,
                &step.to_string(),
                &skill,
                &input_hash,
                &result_hash,
                &timestamp,
            );
            if recomputed != chain_hash {
                anyhow::bail!(
                    "Chain broken at step {} — chain_hash mismatch", step
                );
            }

            // 3. Verify signature
            crypto::verify_signature(&pub_key, chain_hash.as_bytes(), &signature)
                .map_err(|e| anyhow::anyhow!("Bad signature at step {}: {}", step, e))?;

            expected_prev   = chain_hash.clone();
            last_chain_hash = chain_hash;
            count += 1;
        }

        // 4. If task is sealed, final_hash must match last step's chain_hash
        if db_status != "running" {
            if let Some(fh) = expected_final {
                if fh != last_chain_hash {
                    anyhow::bail!(
                        "Seal broken — tasks.final_hash does not match last audit step"
                    );
                }
            }
        }

        Ok(count)
    }

    // ── Config ────────────────────────────────────────────────────────────────

    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM config WHERE key = ?")?;
        stmt.bind((1, key))?;
        if let State::Row = stmt.next()? {
            return Ok(Some(stmt.read(0)?));
        }
        Ok(None)
    }

    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut stmt = self
            .conn
            .prepare("INSERT OR REPLACE INTO config (key, value) VALUES (?, ?)")?;
        stmt.bind((1, key))?;
        stmt.bind((2, value))?;
        stmt.next()?;
        Ok(())
    }

    // ── Skills ────────────────────────────────────────────────────────────────

    pub fn install_skill(&self, name: &str, version: &str, wasm_path: &str) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT OR REPLACE INTO skills (name, version, wasm_path) VALUES (?, ?, ?)",
        )?;
        stmt.bind((1, name))?;
        stmt.bind((2, version))?;
        stmt.bind((3, wasm_path))?;
        stmt.next()?;
        Ok(())
    }

    pub fn get_skill(&self, name: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT wasm_path FROM skills WHERE name = ?")?;
        stmt.bind((1, name))?;
        if let State::Row = stmt.next()? {
            return Ok(Some(stmt.read(0)?));
        }
        Ok(None)
    }

    pub fn list_skills(&self) -> anyhow::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT name, version FROM skills ORDER BY name")?;
        let mut skills = Vec::new();
        while let State::Row = stmt.next()? {
            skills.push((stmt.read(0)?, stmt.read(1)?));
        }
        Ok(skills)
    }

    pub fn remove_skill(&self, name: &str) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare("DELETE FROM skills WHERE name = ?")?;
        stmt.bind((1, name))?;
        stmt.next()?;
        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Next 1-based step index for a task.
    fn next_step_index(&self, task_id: &str) -> anyhow::Result<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT count(*) FROM audit_log WHERE task_id = ?")?;
        stmt.bind((1, task_id))?;
        if let State::Row = stmt.next()? {
            let n: i64 = stmt.read(0)?;
            return Ok((n + 1) as usize);
        }
        Ok(1)
    }

    /// chain_hash of the last audit step for a task, or "" if no steps yet.
    fn get_last_step_hash(&self, task_id: &str) -> anyhow::Result<String> {
        let mut stmt = self.conn.prepare(
            "SELECT chain_hash FROM audit_log WHERE task_id = ? ORDER BY step DESC LIMIT 1",
        )?;
        stmt.bind((1, task_id))?;
        if let State::Row = stmt.next()? {
            return Ok(stmt.read(0)?);
        }
        Ok(String::new())
    }

    /// (final_hash, status) for a task row.
    fn get_task_seal(&self, task_id: &str) -> anyhow::Result<(Option<String>, String)> {
        let mut stmt = self
            .conn
            .prepare("SELECT final_hash, status FROM tasks WHERE task_id = ?")?;
        stmt.bind((1, task_id))?;
        if let State::Row = stmt.next()? {
            let fh: Option<String> = stmt.read::<String, _>(0).ok().filter(|s| !s.is_empty());
            let status: String = stmt.read(1)?;
            return Ok((fh, status));
        }
        anyhow::bail!("Task not found: {}", task_id)
    }

    /// Sign `data` with this daemon's identity key. Returns hex signature.
    fn sign_for_agent(&self, agent_did: &str, data: &str) -> anyhow::Result<String> {
        match self.get_identity()? {
            Some((did, _, enc_priv)) if did == agent_did => {
                match crypto::load_signing_key(&did, &enc_priv) {
                    Ok(key) => Ok(crypto::sign_bytes(&key, data.as_bytes())),
                    Err(e) => {
                        tracing::warn!("Could not load signing key: {}", e);
                        Ok(String::new())
                    }
                }
            }
            _ => Ok(String::new()),
        }
    }

    /// Compute the global task chain link for a new task row.
    /// Returns (task_chain_prev_hash, task_chain_sig).
    fn compute_task_chain_link(
        &self,
        agent_did: &str,
        task_id: &str,
        prompt_hash: &str,
        created_at: &str,
    ) -> anyhow::Result<(String, String)> {
        // Get the previous task's chain hash to link against
        let prev = {
            let mut stmt = self.conn.prepare(
                "SELECT task_chain_prev FROM tasks ORDER BY created_at DESC LIMIT 1",
            )?;
            if let State::Row = stmt.next()? {
                stmt.read::<String, _>(0).unwrap_or_default()
            } else {
                String::new() // genesis
            }
        };

        let link_hash = crypto::sha256_hex_str(
            &format!("{}|{}|{}|{}", prev, task_id, prompt_hash, created_at)
        );
        let sig = self.sign_for_agent(agent_did, &link_hash)?;
        Ok((link_hash, sig))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sec  = secs % 60;
    let min  = (secs / 60) % 60;
    let hour = (secs / 3600) % 24;
    let days = secs / 86400;
    let z    = days + 719468;
    let era  = z / 146097;
    let doe  = z % 146097;
    let yoe  = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y    = yoe + era * 400;
    let doy  = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp   = (5 * doy + 2) / 153;
    let d    = doy - (153 * mp + 2) / 5 + 1;
    let m    = if mp < 10 { mp + 3 } else { mp - 9 };
    let y    = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hour, min, sec)
}

/// Generate a UUIDv4 using rand (no uuid crate needed).
fn new_uuid() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant bits
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
