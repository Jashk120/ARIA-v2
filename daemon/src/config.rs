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
    ollama_url: "http://localhost:11434/v1/chat/completions",
    openrouter_model: "google/gemma-4-26b-a4b-it:free",
    ollama_model: "qwen3.5:9b",
};

/// Loaded once at startup from db + skill manifests, lives in memory for the
/// process lifetime. Never hits db again after init.
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
                let value = db
                    .get_config(key)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| entry.default.clone());
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

        Self {
            searxng_url,
            brave_api_key,
            injected_config: injected,
        }
    }
}
