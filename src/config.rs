pub struct AppConfig {
    pub openrouter_url: &'static str,
    pub default_model: &'static str,
}

pub const CONFIG: AppConfig = AppConfig {
    openrouter_url: "https://openrouter.ai/api/v1/chat/completions",
    default_model: "google/gemma-4-31b-it:free",
};

/// Loaded once at startup from db, lives in memory for the process lifetime.
/// Never hits db again after init.
pub struct RuntimeConfig {
    pub searxng_url: String,
    pub brave_api_key: Option<String>,
}

impl RuntimeConfig {
    pub fn load(db: &crate::db::Db) -> Self {
        Self {
            searxng_url: db.get_config("searxng_url")
                .ok()
                .flatten()
                .unwrap_or_else(|| "https://searx.be".to_string()),
            brave_api_key: db.get_config("brave_api_key")
                .ok()
                .flatten()
                .filter(|k| !k.is_empty()),
        }
    }
}