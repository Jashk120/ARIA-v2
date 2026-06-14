//! Host-side skill dispatcher.
//! Loads WASM skill binaries, wires host functions, executes them.
//! Skills are fully self-describing via manifest.toml — core never hardcodes skill logic.

pub mod manifest;
pub mod paths;
pub mod fs_sandbox;
mod wasm_runtime;

use anyhow::{anyhow, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use wasmtime::{Engine, Module};



use crate::db::Db;
use manifest::load_manifest;
use paths::{skill_dir, wasm_path};
use wasm_runtime::run_wasm_instance_async;

// ── Skill Manager ─────────────────────────────────────────────────────────────

pub struct SkillManager {
    engine:  Engine,
    modules: RwLock<HashMap<String, Arc<Module>>>,
}

impl SkillManager {
    pub fn new() -> anyhow::Result<Self> {
        let config = wasmtime::Config::new();
        let engine = Engine::new(&config)?;
        Ok(Self {
            engine,
            modules: RwLock::new(HashMap::new()),
        })
    }

    fn get_module(&self, name: &str, path: &Path) -> anyhow::Result<Arc<Module>> {
        {
            let modules = self.modules.read().map_err(|_| anyhow!("Lock poisoned"))?;
            if let Some(m) = modules.get(name) {
                return Ok(Arc::clone(m));
            }
        }

        let module = Module::from_file(&self.engine, path)?;
        let module = Arc::new(module);

        let mut modules = self.modules.write().map_err(|_| anyhow!("Lock poisoned"))?;
        modules.insert(name.to_string(), Arc::clone(&module));
        Ok(module)
    }

    /// Run a skill with db key injection.
    pub async fn run_skill(&self, name: &str, args: &Value, db: &Db) -> anyhow::Result<Value> {
        let mut args = args.clone();
        enrich_args(name, &mut args, db)?;
        self.run_skill_raw(name, &args).await
    }

    /// Run a skill with pre-enriched args.
    pub async fn run_skill_raw(&self, name: &str, args: &Value) -> anyhow::Result<Value> {
        let path = wasm_path(name)?;

        if !path.exists() {
            bail!(
                "Skill '{}' not found at {}.\nBuild it with: cargo build -p {}_{} --target wasm32-wasip1 --release",
                name,
                path.display(),
                name.split('.').next().unwrap_or(name),
                name.split('.').last().unwrap_or(name),
            );
        }

        let dir      = skill_dir(name)?;
        let manifest = load_manifest(&dir)?;
        let module   = self.get_module(name, &path)?;
        let engine   = self.engine.clone();

        run_wasm_instance_async(&engine, module, args, &manifest).await
    }
}

// ── Config injection ─────────────────────────────────────────────────────────

/// Inject config keys into skill args based on manifest [config] entries with inject=true.
/// This covers both runtime-config values (searxng_url, fs_root, ...) used by host
/// functions, and any values the skill itself reads from its args.
fn enrich_args(skill: &str, args: &mut Value, db: &Db) -> anyhow::Result<()> {
    let dir      = skill_dir(skill)?;
    let manifest = load_manifest(&dir)?;

    let obj = args.as_object_mut()
        .ok_or_else(|| anyhow!("Args must be a JSON object to enrich keys"))?;

    for (key, entry) in &manifest.config {
        if !entry.inject {
            continue;
        }
        let value = db.get_config(key)
            .ok()
            .flatten()
            .unwrap_or_else(|| entry.default.clone());

        obj.insert(key.clone(), Value::String(value));
    }

    Ok(())
}

// ── UI helpers ────────────────────────────────────────────────────────────────

pub fn describe_action(skill: &str, args: &Value) -> String {
    if let Ok(dir) = skill_dir(skill) {
        if let Ok(manifest) = load_manifest(&dir) {
            if let Some(template) = manifest.display.action {
                return render_template(&template, args);
            }
        }
    }
    format!("Running skill: {}", skill)
}

fn render_template(template: &str, args: &Value) -> String {
    let mut result = template.to_string();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            let placeholder = format!("{{{}}}", k);
            let value = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b)   => b.to_string(),
                _ => "...".to_string(),
            };
            result = result.replace(&placeholder, &value);
        }
    }
    result
}
