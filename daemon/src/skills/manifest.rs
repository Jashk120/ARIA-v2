//! Canonical manifest.toml schema.
//! Used by both the runtime (skill execution / capability wiring) and the
//! agent prompt builder (skill index + trigger matching).

use anyhow::anyhow;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Debug, Clone)]
pub struct SkillManifest {
    pub name:         String,
    pub version:      String,
    pub description:  String,
    /// Substring triggers used by the agent prompt builder to decide whether
    /// to show this skill's full call/output schema. Empty = always shown in full.
    #[serde(default)]
    pub triggers:     Vec<String>,
    #[serde(default)]
    pub display:      DisplayConfig,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub call:         CallConfig,
    #[serde(default)]
    pub react:        ReactConfig,
    #[serde(default)]
    pub config:       HashMap<String, ConfigEntry>,
}

#[derive(Deserialize, Default, Debug, Clone)]
pub struct DisplayConfig {
    /// Template string, e.g. "Searching the web for \"{query}\""
    /// {key} is replaced with args[key] at runtime
    pub action: Option<String>,
}

#[derive(Deserialize, Default, Debug, Clone)]
pub struct Capabilities {
    #[serde(default)]
    pub http: bool,
    #[serde(default)]
    pub fs:   bool,
}

#[derive(Deserialize, Default, Debug, Clone)]
pub struct CallConfig {
    /// JSON shape the LLM should pass as args — shown verbatim in prompt
    pub args_schema:   Option<String>,
    /// JSON shape the skill returns — so LLM can reason about observations
    pub output_schema: Option<String>,
}

#[derive(Deserialize, Default, Debug, Clone)]
pub struct ReactConfig {
    /// Max times this skill may fire in one turn (default: unlimited within MAX_REACT_STEPS)
    pub max_steps: Option<usize>,
    /// If true, skill output is the final answer — skip LLM synthesis pass
    #[serde(default)]
    pub terminal:  bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ConfigEntry {
    #[serde(default)]
    pub default: String,
    /// If true, host injects this key from db into skill args
    #[serde(default)]
    pub inject: bool,
    /// If true, never log this value
    #[serde(default)]
    pub secret: bool,
}

pub fn load_manifest(skill_dir: &Path) -> anyhow::Result<SkillManifest> {
    let path = skill_dir.join("manifest.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("Missing manifest.toml at {}", path.display()))?;
    toml::from_str(&text).map_err(|e| anyhow!("Invalid manifest.toml: {}", e))
}
