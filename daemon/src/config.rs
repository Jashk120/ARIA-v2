use std::collections::HashMap;

use crate::db::Db;

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

/// Wire protocol used to talk to the LLM endpoint. OpenCode Go exposes one
/// gateway base but three protocols, selected by model family.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Protocol {
    #[default]
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

/// Select the wire protocol for a provider/model pair. Only `opencode-go`
/// models ever leave chat-completions: `grok-*`/`gpt-*`/`muse-spark-*` use
/// the Responses API, `claude-*`/`qwen3.8*`/`qwen3.7*`/`minimax-m3*`/
/// `minimax-m2.7*` use the Anthropic Messages API.
pub fn protocol_for(provider_id: &str, model: &str) -> Protocol {
    if provider_id != "opencode-go" {
        return Protocol::ChatCompletions;
    }
    let m = model.trim().to_lowercase();
    if m.starts_with("grok-") || m.starts_with("gpt-") || m.starts_with("muse-spark-") {
        Protocol::Responses
    } else if m.starts_with("claude-")
        || m.starts_with("qwen3.8")
        || m.starts_with("qwen3.7")
        || m.starts_with("minimax-m3")
        || m.starts_with("minimax-m2.7")
    {
        Protocol::AnthropicMessages
    } else {
        Protocol::ChatCompletions
    }
}

/// Path appended to the gateway base for a protocol.
pub fn protocol_path(p: Protocol) -> &'static str {
    match p {
        Protocol::ChatCompletions => "/chat/completions",
        Protocol::Responses => "/responses",
        Protocol::AnthropicMessages => "/messages",
    }
}

/// First-class provider preset: a named bundle of endpoint + auth env +
/// default model + known model list surfaced to the GUI.
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub url: &'static str,
    pub api_key_env: &'static str,
    pub default_model: &'static str,
    pub models: &'static [&'static str],
}

pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "local",
        name: "Local (LiteLLM / Ollama)",
        url: "http://127.0.0.1:8000/v1/chat/completions",
        api_key_env: "",
        default_model: "gemma-4-31b-it",
        models: &[
            "gemma-4-31b-it",
            "gemma-4-26b-a4b-it",
            "gemini/gemini-3.5-flash",
            "gemini/gemma-4-31b-it",
        ],
    },
    ProviderPreset {
        id: "openrouter",
        name: "OpenRouter",
        url: "https://openrouter.ai/api/v1/chat/completions",
        api_key_env: "OPENROUTER_API_KEY",
        default_model: "google/gemma-4-26b-a4b-it:free",
        models: &["google/gemma-4-26b-a4b-it:free"],
    },
    ProviderPreset {
        id: "opencode-go",
        name: "OpenCode Go",
        url: "https://opencode.ai/zen/go/v1",
        api_key_env: "OPENCODE_API_KEY",
        default_model: "deepseek-v4.1-flash",
        models: &[
            "deepseek-v4.1-flash",
            "deepseek-v4-pro",
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
            "glm-5.3",
            "glm-5.3-flash",
            "glm-5.2",
            "kimi-k3",
            "kimi-k2.7-code",
            "mimo-v2.6-pro",
            "mimo-v2.6-flash",
            "mimo-v2.5",
            "mimo-v2.5-pro",
            "longcat-2.5-preview-free",
            "longcat-2.0",
            "hy4-preview",
            "hy3",
            "space-bunny",
            "grok-4.7",
            "grok-4.6",
            "gpt-6-luna",
            "gpt-5.6-luna",
            "muse-spark-1.3-contributor",
            "muse-spark-1.2-contributor",
            "claude-haiku-5-5",
            "qwen3.8-max",
            "qwen3.8-flash",
            "qwen3.7-plus",
            "minimax-m3",
            "minimax-m2.7",
        ],
    },
];

/// Look up a provider preset by id. Empty/unknown ids match nothing.
pub fn find_preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|p| p.id == id)
}

/// DB-aware LLM settings. DB/GUI values win, then env, then compiled default.
pub struct LlmSettings {
    pub url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub protocol: Protocol,
    pub provider_id: String,
}

impl LlmSettings {
    pub fn is_opencode_go(&self) -> bool {
        self.provider_id == "opencode-go"
    }
}

/// Replace every literal `{TOKEN}` in `template` with `token` (empty when `None`).
pub fn apply_template(template: &str, token: Option<&str>) -> String {
    template.replace("{TOKEN}", token.unwrap_or_default())
}

fn db_value(db: &Db, key: &str) -> Option<String> {
    db.get_config(key).ok().flatten().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn is_go_base(url: &str) -> bool {
    url.trim().contains("opencode.ai/zen/go")
}

/// Build the per-protocol Go endpoint from a gateway base: strip any
/// trailing protocol path, then append the one for `protocol`.
fn go_endpoint(base: &str, protocol: Protocol) -> String {
    let mut trimmed = base.trim().to_string();
    for suffix in ["/chat/completions", "/responses", "/messages"] {
        if let Some(stripped) = trimmed.strip_suffix(suffix) {
            trimmed = stripped.to_string();
            break;
        }
    }
    format!("{}{}", trimmed.trim_end_matches('/'), protocol_path(protocol))
}

/// Resolve LLM settings from the GUI config table with env/default fallbacks.
///
/// Precedence:
/// - provider id: db `llm_provider` (empty/absent = no preset)
/// - url: db `llm_url_template` (`{TOKEN}` via `llm_token`) > db `llm_url`
///   (a Go-base value is re-pathed per protocol, anything else normalized
///   to chat-completions) > preset url (same treatment) > `llm_url()`
/// - model: db `llm_model` > preset default model > `llm_model()`
/// - api_key: db `llm_api_key` > preset `api_key_env` env var > None
/// - protocol: `protocol_for(provider_id, model)`
pub fn resolve_llm(db: &Db) -> LlmSettings {
    let provider_id = db_value(db, "llm_provider").unwrap_or_default();
    let preset = if provider_id.is_empty() { None } else { find_preset(&provider_id) };
    let token = db_value(db, "llm_token");
    let model = match db_value(db, "llm_model") {
        Some(m) => m,
        None => match preset {
            Some(p) => p.default_model.to_string(),
            None => llm_model(),
        },
    };
    let protocol = protocol_for(&provider_id, &model);
    let is_go = provider_id == "opencode-go";
    let url = match db_value(db, "llm_url_template") {
        Some(template) => {
            if template.contains("{TOKEN}") && token.as_deref().unwrap_or_default().is_empty() {
                tracing::warn!(
                    "llm_url_template contains {{TOKEN}} but llm_token is missing; substituting an empty string"
                );
            }
            apply_template(&template, token.as_deref())
        }
        None => match db_value(db, "llm_url") {
            Some(raw) => {
                if is_go && is_go_base(&raw) {
                    go_endpoint(&raw, protocol)
                } else {
                    normalize_llm_url(&raw)
                }
            }
            None => match preset {
                Some(p) if is_go => go_endpoint(p.url, protocol),
                Some(p) => normalize_llm_url(p.url),
                None => llm_url(),
            },
        },
    };
    let api_key = match db_value(db, "llm_api_key") {
        Some(k) => Some(k),
        None => match preset {
            Some(p) if !p.api_key_env.is_empty() => std::env::var(p.api_key_env)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            _ => None,
        },
    };
    LlmSettings { url, model, api_key, protocol, provider_id }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_without_placeholder_is_unchanged() {
        assert_eq!(apply_template("https://example.com/chat", None), "https://example.com/chat");
    }

    #[test]
    fn template_with_placeholder_and_token() {
        assert_eq!(
            apply_template("https://example.com/{TOKEN}/chat", Some("secret")),
            "https://example.com/secret/chat"
        );
    }

    #[test]
    fn protocol_for_classifies_opencode_go_families() {
        for model in ["grok-4.7", "gpt-6-luna", "muse-spark-1.3-contributor", "MUSE-SPARK-1.2-X"] {
            assert_eq!(protocol_for("opencode-go", model), Protocol::Responses, "{model}");
        }
        for model in ["claude-haiku-5-5", "qwen3.8-max", "qwen3.7-plus", "minimax-m3", "minimax-m2.7"] {
            assert_eq!(protocol_for("opencode-go", model), Protocol::AnthropicMessages, "{model}");
        }
        for model in ["deepseek-v4.1-flash", "glm-5.3", "kimi-k3", "hy3", "space-bunny"] {
            assert_eq!(protocol_for("opencode-go", model), Protocol::ChatCompletions, "{model}");
        }
        assert_eq!(protocol_for("local", "grok-4.7"), Protocol::ChatCompletions);
        assert_eq!(protocol_for("", "claude-haiku-5-5"), Protocol::ChatCompletions);
        assert_eq!(protocol_path(Protocol::ChatCompletions), "/chat/completions");
        assert_eq!(protocol_path(Protocol::Responses), "/responses");
        assert_eq!(protocol_path(Protocol::AnthropicMessages), "/messages");
    }

    #[test]
    fn resolve_llm_precedence_db_template_and_key() -> anyhow::Result<()> {
        let db = Db::open_test()?;
        db.set_config("llm_provider", "opencode-go")?;
        db.set_config("llm_url_template", "https://example.com/{TOKEN}/chat")?;
        db.set_config("llm_token", "tok123")?;
        db.set_config("llm_api_key", "db-key")?;
        let settings = resolve_llm(&db);
        assert_eq!(settings.url, "https://example.com/tok123/chat");
        assert_eq!(settings.model, "deepseek-v4.1-flash");
        assert_eq!(settings.api_key.as_deref(), Some("db-key"));
        Ok(())
    }

    #[test]
    fn resolve_llm_go_base_repathed_per_protocol() -> anyhow::Result<()> {
        let db = Db::open_test()?;
        db.set_config("llm_provider", "opencode-go")?;
        db.set_config("llm_model", "grok-4.7")?;
        let settings = resolve_llm(&db);
        assert_eq!(settings.url, "https://opencode.ai/zen/go/v1/responses");
        assert_eq!(settings.protocol, Protocol::Responses);
        assert!(settings.is_opencode_go());
        db.set_config("llm_model", "claude-haiku-5-5")?;
        let settings = resolve_llm(&db);
        assert_eq!(settings.url, "https://opencode.ai/zen/go/v1/messages");
        db.set_config("llm_url", "https://opencode.ai/zen/go/v1/chat/completions")?;
        db.set_config("llm_model", "deepseek-v4.1-flash")?;
        let settings = resolve_llm(&db);
        assert_eq!(settings.url, "https://opencode.ai/zen/go/v1/chat/completions");
        Ok(())
    }

    #[test]
    fn resolve_llm_db_url_overrides_preset_and_falls_back_to_preset_env_key(
    ) -> anyhow::Result<()> {
        let db = Db::open_test()?;
        db.set_config("llm_provider", "opencode-go")?;
        db.set_config("llm_url", "http://localhost:9000")?;
        unsafe { std::env::set_var("OPENCODE_API_KEY", "env-key") };
        let settings = resolve_llm(&db);
        assert_eq!(settings.url, "http://localhost:9000/v1/chat/completions");
        assert_eq!(settings.model, "deepseek-v4.1-flash");
        assert_eq!(settings.api_key.as_deref(), Some("env-key"));
        unsafe { std::env::remove_var("OPENCODE_API_KEY") };
        Ok(())
    }
}
