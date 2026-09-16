use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq)]
pub enum Provider {
    Ollama,
    OpenRouter,
}

pub struct AppConfig {
    pub use_provider: Provider,
    pub openrouter_url: &'static str,
    pub ollama_url: &'static str,
    pub openrouter_model: &'static str,
    pub ollama_model: &'static str,
}

pub const CONFIG: AppConfig = AppConfig {
    use_provider: Provider::Ollama,
    openrouter_url: "https://openrouter.ai/api/v1/chat/completions",
    ollama_url: "http://127.0.0.1:8000/v1/chat/completions", //"http://localhost:11434/v1/chat/completions",
    openrouter_model: "google/gemma-4-26b-a4b-it:free",
    ollama_model: "gemma-4-31b-it",
};

/// Resolve the LLM chat-completions endpoint.
///
/// Precedence: `ARIA_LLM_URL` > `OPENAI_BASE_URL` > `LITELLM_BASE_URL` > compiled
/// default. Bare host:port values get `/v1/chat/completions` appended so
/// `http://localhost:8000` just works. A `0.0.0.0` host (bind address, not a
/// routable destination) is rewritten to `127.0.0.1`.
pub fn llm_url() -> String {
    let from_env = ["ARIA_LLM_URL", "OPENAI_BASE_URL", "LITELLM_BASE_URL"]
        .iter()
        .find_map(|v| {
            std::env::var(v).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        });
    let raw = from_env.unwrap_or_else(|| match CONFIG.use_provider {
        Provider::OpenRouter => CONFIG.openrouter_url.to_string(),
        Provider::Ollama => CONFIG.ollama_url.to_string(),
    });
    normalize_llm_url(&raw)
}

/// Resolve the LLM model name. `ARIA_LLM_MODEL` > `OPENAI_MODEL` > default.
pub fn llm_model() -> String {
    ["ARIA_LLM_MODEL", "OPENAI_MODEL"]
        .iter()
        .find_map(|v| {
            std::env::var(v).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| match CONFIG.use_provider {
            Provider::OpenRouter => CONFIG.openrouter_model.to_string(),
            Provider::Ollama => CONFIG.ollama_model.to_string(),
        })
}

fn normalize_llm_url(raw: &str) -> String {
    let mut url = raw.trim().to_string();
    // 0.0.0.0 is a listen address, not a dial address.
    url = url.replace("0.0.0.0", "127.0.0.1");
    if !url.contains("/v1/chat/completions") {
        let base = url.trim_end_matches('/');
        if base.ends_with("/v1") {
            url = format!("{}/chat/completions", base);
        } else if !base.contains("/v1/") {
            url = format!("{}/v1/chat/completions", base);
        }
    }
    url
}

/// Fallback candidates when the primary endpoint refuses connections.
/// If primary is :8000, also try :4000 (and vice versa), plus a
/// localhost<->127.0.0.1 swap. Deduped, primary first.
pub fn llm_candidates(primary: &str) -> Vec<String> {
    let mut out = vec![primary.to_string()];
    let swap_port = if primary.contains(":8000") {
        Some((":8000", ":4000"))
    } else if primary.contains(":4000") {
        Some((":4000", ":8000"))
    } else {
        None
    };
    if let Some((from, to)) = swap_port {
        out.push(primary.replacen(from, to, 1));
    }
    if primary.contains("127.0.0.1") {
        out.push(primary.replacen("127.0.0.1", "localhost", 1));
    } else if primary.contains("localhost") {
        out.push(primary.replacen("localhost", "127.0.0.1", 1));
    }
    out.sort();
    out.dedup();
    // Keep primary first.
    out.sort_by_key(|u| if u == primary { 0 } else { 1 });
    out
}

/// Loaded once at startup from db + skill manifests, lives in memory for the
/// process lifetime. Never hits db again after init.
#[derive(Clone)]
pub struct RuntimeConfig {
    /// Backward-compat convenience accessors
    pub searxng_url: String,
    pub brave_api_key: Option<String>,

    /// Generic map of ALL inject=true config keys across all skill manifests.
    /// Keys that exist in the db override the manifest default.
    /// This is what the react loop uses for config injection — any new skill
    /// with [config.X] inject=true will automatically be picked up here
    /// without touching react_loop.rs or repl.rs.
    pub injected_config: HashMap<String, HashMap<String, String>>,
    /// Payment governance static config
    pub governance: PaymentGovernanceConfig,
}

#[derive(Debug, Clone)]
pub struct PaymentGovernanceConfig {
    pub per_task_cap: Option<f64>,
    pub per_day_cap: Option<f64>,
    pub auto_under: Option<f64>,
    pub audit_topic_id: Option<String>,
    /// Air-gap master switch for all DLT skills (`hedera_pay` / `x402_pay`)
    /// plus HCS audit egress. Default ON (true) — absent/empty config means
    /// enabled, so fresh installs behave exactly as before this flag existed.
    pub dlt_enabled: bool,
}

impl Default for PaymentGovernanceConfig {
    fn default() -> Self {
        Self {
            per_task_cap: None,
            per_day_cap: None,
            auto_under: None,
            audit_topic_id: None,
            // Default-true preserves current behavior for any constructor
            // that doesn't set this field explicitly (same family as the
            // per_task_cap/per_day_cap governance knobs).
            dlt_enabled: true,
        }
    }
}

/// Parse a user-supplied boolean for the `dlt_enabled` flag.
///
/// Returns `None` for absent/empty/unrecognized input (caller falls through
/// to the next source, ultimately defaulting to enabled). Only explicit
/// `0`/`false`/`off`/`no` disables; explicit `1`/`true`/`on`/`yes` enables.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_lowercase().as_str() {
        "" => None,
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Effective `dlt_enabled` value from an env override + a db value.
/// Env (`ARIA_DLT_ENABLED`) wins when it parses; otherwise the db key
/// (`dlt_enabled`) wins when it parses; otherwise enabled (default ON).
pub fn dlt_enabled_from_sources(env_val: Option<&str>, db_val: Option<&str>) -> bool {
    if let Some(raw) = env_val
        && let Some(parsed) = parse_bool(raw) {
            return parsed;
        }
    if let Some(raw) = db_val
        && let Some(parsed) = parse_bool(raw) {
            return parsed;
        }
    true
}

/// Live read of the air-gap flag: env `ARIA_DLT_ENABLED` first, then the
/// db `dlt_enabled` key, defaulting to enabled. Enforcement points
/// (wasm host gating, react-loop proposal guard, `query_dlt_status`) call
/// this on every use instead of trusting the startup-cached
/// `RuntimeConfig` clone — that clone is built once in `run_daemon` and
/// never refreshed, so reading through it would NOT be live. This single
/// key read is what makes the CLI/GUI toggle take effect without a
/// daemon restart; no watcher is needed.
pub fn dlt_enabled_live(db: &crate::db::Db) -> bool {
    let env_val = std::env::var("ARIA_DLT_ENABLED").ok();
    let db_val = db.get_config("dlt_enabled").ok().flatten();
    dlt_enabled_from_sources(env_val.as_deref(), db_val.as_deref())
}

/// Single code path for flipping the air-gap flag, shared by the
/// `aria dlt on|off` CLI commands and the TCP `mutate_dlt` endpoint so the
/// two can never diverge. Stored as `"1"`/`"0"`; an unset key reads back
/// as enabled via `dlt_enabled_live`.
pub fn set_dlt_enabled(db: &crate::db::Db, enabled: bool) -> anyhow::Result<()> {
    db.set_config("dlt_enabled", if enabled { "1" } else { "0" })
}

impl RuntimeConfig {
    pub fn load(db: &crate::db::Db) -> Self {
        // 1. Load all skill manifests and collect inject=true keys.
        let all_skills = crate::agent::prompt::load_all_skills();
        let mut injected: HashMap<String, HashMap<String, String>> = HashMap::new();

        for manifest in &all_skills {
            let mut skill_config = HashMap::new();
            for (key, entry) in &manifest.config {
                if !entry.inject {
                    continue;
                }
                let value =
                    db.get_config(key).ok().flatten().unwrap_or_else(|| entry.default.clone());
                skill_config.insert(key.clone(), value);
            }
            if !skill_config.is_empty() {
                injected.insert(manifest.name.clone(), skill_config);
            }
        }

        // 2. Extract well-known keys for backward-compat display.
        let searxng_url = injected
            .get("search.web")
            .and_then(|c| c.get("searxng_url"))
            .cloned()
            .unwrap_or_else(|| "https://searx.be".to_string());
        let brave_api_key = injected
            .get("search.web")
            .and_then(|c| c.get("brave_api_key"))
            .cloned()
            .filter(|k| !k.is_empty());

        let parse_opt_f64 = |env_var: &str, db_key: &str| -> Option<f64> {
            if let Ok(v) = std::env::var(env_var) {
                if let Ok(n) = v.trim().parse::<f64>() {
                    return Some(n);
                }
            }
            if let Ok(Some(v)) = db.get_config(db_key) {
                if let Ok(n) = v.trim().parse::<f64>() {
                    return Some(n);
                }
            }
            None
        };

        let parse_opt_str = |env_vars: &[&str], db_key: &str| -> Option<String> {
            for var in env_vars {
                if let Ok(v) = std::env::var(var) {
                    if !v.trim().is_empty() {
                        return Some(v.trim().to_string());
                    }
                }
            }
            if let Ok(Some(v)) = db.get_config(db_key) {
                if !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
            None
        };

        let governance = PaymentGovernanceConfig {
            per_task_cap: parse_opt_f64("PER_TASK_CAP", "per_task_cap")
                .or_else(|| parse_opt_f64("ARIA_PER_TASK_CAP", "aria_per_task_cap")),
            per_day_cap: parse_opt_f64("PER_DAY_CAP", "per_day_cap")
                .or_else(|| parse_opt_f64("ARIA_PER_DAY_CAP", "aria_per_day_cap")),
            auto_under: parse_opt_f64("AUTO_UNDER", "auto_under")
                .or_else(|| parse_opt_f64("ARIA_AUTO_UNDER", "aria_auto_under")),
            audit_topic_id: parse_opt_str(
                &["HEDERA_PAYMENT_AUDIT_TOPIC", "HEDERA_AUDIT_TOPIC", "ARIA_AUDIT_TOPIC"],
                "hedera_payment_audit_topic",
            ),
            // Air-gap flag: env ARIA_DLT_ENABLED wins, then db "dlt_enabled",
            // default ON. Same env-override-then-db pattern as the caps above.
            dlt_enabled: dlt_enabled_from_sources(
                std::env::var("ARIA_DLT_ENABLED").ok().as_deref(),
                db.get_config("dlt_enabled").ok().flatten().as_deref(),
            ),
        };

        Self { searxng_url, brave_api_key, injected_config: injected, governance }
    }
}
