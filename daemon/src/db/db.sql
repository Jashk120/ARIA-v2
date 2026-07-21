-- Core identity. One row per registered agent on this device.
-- "did:aria:jayesh" hardcoded Phase 1
-- Phase 3: did is did:aria:..., vc populated after server registration
CREATE TABLE identity (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    did             TEXT UNIQUE NOT NULL,       -- "did:aria:jayesh" hardcoded Phase 1
                                                -- did:aria:acme:alice:assistant (Phase 3)
    public_key      TEXT NOT NULL,              -- base58/multibase encoded Ed25519
    private_key     TEXT NOT NULL,              -- encrypted at rest
    manifest_path   TEXT NOT NULL,              -- path to manifest.toml
    vc              TEXT,                       -- NULL Phase 1, signed VC JSON Phase 3
    vp_template     TEXT,                       -- NULL Phase 1, VP template Phase 3
    created_at      TEXT DEFAULT CURRENT_TIMESTAMP,
    updated_at      TEXT DEFAULT CURRENT_TIMESTAMP
);

-- Cached manifests from other agents (for future verification)
-- Phase 1: unused. Phase 3: populated when verifying incoming messages.
CREATE TABLE cached_manifests (
    did             TEXT PRIMARY KEY,
    manifest        TEXT NOT NULL,
    verified        INTEGER DEFAULT 0,          -- 0=unverified, 1=verified Phase 3
    cached_at       TEXT DEFAULT CURRENT_TIMESTAMP,
    expires_at      TEXT
);

-- Every skill call, autonomous or user-triggered.
-- Phase 1: signature is NULL, prev_hash is NULL (no chaining yet)
-- Phase 3: every entry signed + hash-chained
CREATE TABLE audit_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_did       TEXT NOT NULL,
    trigger         TEXT NOT NULL,              -- 'cli' | 'watch' | 'cron' | 'webhook'
    skill_called    TEXT,
    input_hash      TEXT,                       -- hash of input, not plaintext
    result_hash     TEXT,                       -- hash of result, not plaintext
    prev_hash       TEXT,                       -- NULL Phase 1, chain hash Phase 3
    signature       TEXT,                       -- NULL Phase 1, Ed25519 Phase 3
    success         INTEGER NOT NULL DEFAULT 1,
    timestamp       TEXT DEFAULT CURRENT_TIMESTAMP
);

-- Conversation history per agent
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_did       TEXT NOT NULL,
    direction       TEXT NOT NULL,              -- 'user' | 'agent'
    content         TEXT NOT NULL,
    skill_calls     TEXT,                       -- JSON array of skills called this turn
    timestamp       TEXT DEFAULT CURRENT_TIMESTAMP
);

-- Capability requests — skills the agent tried to call but weren't in manifest
-- Phase 1: logged locally. Phase 3: synced to identity server as product signal.
CREATE TABLE capability_requests (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_did       TEXT NOT NULL,
    skill_requested TEXT NOT NULL,
    context         TEXT,                       -- what the agent was trying to do
    synced          INTEGER DEFAULT 0,          -- 0=local only, 1=synced to server
    timestamp       TEXT DEFAULT CURRENT_TIMESTAMP
);
-- Installed skills (user-installed via `aria skill install`)
CREATE TABLE skills (
    name            TEXT PRIMARY KEY,           -- "search.web"
    version         TEXT NOT NULL,
    wasm_path       TEXT NOT NULL,              -- ~/.aria/skills/web/search.web/search_web.wasm
    manifest        TEXT,                       -- JSON of manifest.toml
    installed_at    TEXT DEFAULT CURRENT_TIMESTAMP
);

-- x402 payment transactions made by the agent (Hedera testnet/mainnet).
-- Phase 1 (bounty): unlinked to audit_log by id, just recipient/amount/receipt.
-- Phase 3: link back to audit_log.id once hash-chaining covers payments too.
CREATE TABLE payments (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_did       TEXT NOT NULL,
    skill_called    TEXT,                       -- e.g. "pay.x402"
    recipient       TEXT NOT NULL,               -- Hedera AccountId
    amount_hbar     REAL NOT NULL,
    memo            TEXT,
    transaction_id  TEXT NOT NULL,               -- Hedera tx id
    hashscan_url    TEXT NOT NULL,
    status          TEXT NOT NULL,               -- receipt status, e.g. "SUCCESS"
    timestamp       TEXT DEFAULT CURRENT_TIMESTAMP
);