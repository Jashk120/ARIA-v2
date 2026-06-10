pub struct AppConfig {
    pub openrouter_url: &'static str,
    pub default_model: &'static str,
}

pub const CONFIG: AppConfig = AppConfig {
    openrouter_url: "https://openrouter.ai/api/v1/chat/completions",
    default_model: "google/gemma-4-31b-it:free",
};
