//! Host-side skill dispatcher.
//! Loads WASM skill binaries, wires host functions, executes them.
//! Skills are fully self-describing via manifest.toml — core never hardcodes skill logic.

use anyhow::{anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use wasmtime::{Caller, Engine, Linker, Module, Store};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

use crate::db::Db;

// ── Manifest ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SkillManifest {
    name:         String,
    version:      String,
    description:  String,
    #[serde(default)]
    display:      DisplayConfig,
    #[serde(default)]
    capabilities: Capabilities,
    #[serde(default)]
    config:       HashMap<String, ConfigEntry>,
}

#[derive(Deserialize, Default)]
struct DisplayConfig {
    /// Template string, e.g. "Searching the web for \"{query}\""
    /// {key} is replaced with args[key] at runtime
    action: Option<String>,
}

#[derive(Deserialize, Default)]
struct Capabilities {
    #[serde(default)]
    http: bool,
}

#[derive(Deserialize)]
struct ConfigEntry {
    #[serde(default)]
    default: String,
    /// If true, host injects this key from db into skill args
    #[serde(default)]
    inject: bool,
    /// If true, never log this value
    #[serde(default)]
    secret: bool,
}

fn load_manifest(skill_dir: &PathBuf) -> anyhow::Result<SkillManifest> {
    let path = skill_dir.join("manifest.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("Missing manifest.toml at {}", path.display()))?;
    toml::from_str(&text).map_err(|e| anyhow!("Invalid manifest.toml: {}", e))
}

// ── Path resolution ───────────────────────────────────────────────────────────

fn skill_dir(name: &str) -> anyhow::Result<PathBuf> {
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    if parts.len() != 2 {
        bail!("Invalid skill name '{}' — expected format: action.category", name);
    }
    let (action, category) = (parts[0], parts[1]);

    let exe = std::env::current_exe()?;
    let root = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("Could not resolve workspace root"))?;

    Ok(root
        .join("skills")
        .join(category)
        .join(format!("{}.{}", action, category)))
}

fn wasm_path(name: &str) -> anyhow::Result<PathBuf> {
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    let (action, category) = (parts[0], parts[1]);

    let exe = std::env::current_exe()?;
    let root = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("Could not resolve workspace root"))?;

    Ok(root
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join(format!("{}_{}.wasm", action, category)))
}

// ── Host state ────────────────────────────────────────────────────────────────

struct HostState {
    http_client: reqwest::blocking::Client,
    wasi:        WasiP1Ctx,
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Run a skill with db key injection.
pub async fn run_skill(name: &str, args: &Value, db: &Db) -> anyhow::Result<Value> {
    let mut args = args.clone();
    enrich_args(name, &mut args, db)?;
    run_skill_raw(name, &args).await
}

/// Run a skill with pre-enriched args — no db access needed.
pub async fn run_skill_raw(name: &str, args: &Value) -> anyhow::Result<Value> {
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
    let args_json = args.to_string();

    let result = tokio::task::spawn_blocking(move || {
        run_wasm(&path, &args_json, &manifest)
    }).await??;

    Ok(result)
}

/// Inject config keys into skill args based on manifest.
fn enrich_args(skill: &str, args: &mut Value, db: &Db) -> anyhow::Result<()> {
    let dir      = skill_dir(skill)?;
    let manifest = load_manifest(&dir)?;

    for (key, entry) in &manifest.config {
        if !entry.inject {
            continue;
        }
        let value = db.get_config(key)
            .ok()
            .flatten()
            .unwrap_or_else(|| entry.default.clone());

        if !entry.secret {
            // only log non-secret injections
        }
        args[key] = Value::String(value);
    }

    Ok(())
}

// ── WASM execution ────────────────────────────────────────────────────────────

fn run_wasm(path: &PathBuf, args_json: &str, manifest: &SkillManifest) -> anyhow::Result<Value> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, path)?;

    let wasi  = WasiCtxBuilder::new().build_p1();
    let state = HostState {
        http_client: reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
        wasi,
    };

    let mut store  = Store::new(&engine, state);
    let mut linker: Linker<HostState> = Linker::new(&engine);

    p1::add_to_linker_sync(&mut linker, |s| &mut s.wasi)?;

    // Wire capabilities declared in manifest only
    if manifest.capabilities.http {
        linker.func_wrap(
            "aria",
            "host_http_get",
            |mut caller: Caller<'_, HostState>,
             url_ptr: i32, url_len: i32,
             headers_ptr: i32, headers_len: i32|
             -> i32 {
                let url = match read_wasm_str(&mut caller, url_ptr, url_len) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[host_http_get] failed to read url: {}", e); return 0; }
                };
                let headers_json = match read_wasm_str(&mut caller, headers_ptr, headers_len) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[host_http_get] failed to read headers: {}", e); return 0; }
                };

                eprintln!("[host_http_get] GET {}", url);

                match do_http_get(&caller.data().http_client, &url, &headers_json) {
                    Ok(body) => {
                        eprintln!("[host_http_get] OK {} bytes", body.len());
                        write_wasm_bytes(&mut caller, &body).unwrap_or(0)
                    }
                    Err(e) => {
                        eprintln!("[host_http_get] FAILED: {}", e);
                        0
                    }
                }
            },
        )?;
    }

    // host_free always wired — skills call it unconditionally
    linker.func_wrap("aria", "host_free", |_: Caller<'_, HostState>, _ptr: i32| {})?;

    let instance = linker.instantiate(&mut store, &module)?;
    let memory   = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("Skill has no exported memory"))?;

    let args_bytes = args_json.as_bytes();
    memory.write(&mut store, 0, args_bytes)?;

    let run_fn     = instance.get_typed_func::<(i32, i32), i32>(&mut store, "run")?;
    let result_ptr = run_fn.call(&mut store, (0, args_bytes.len() as i32))?;
    let result_str = read_wasm_cstr(&store, &memory, result_ptr as usize)?;

    let result: Value = serde_json::from_str(&result_str)
        .map_err(|e| anyhow!("Skill returned invalid JSON: {} — raw: {}", e, result_str))?;

    if let Some(err) = result["error"].as_str() {
        bail!("Skill error: {}", err);
    }

    Ok(result)
}

// ── Memory helpers ────────────────────────────────────────────────────────────

fn read_wasm_str(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> anyhow::Result<String> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| anyhow!("No memory export"))?;
    let data  = memory.data(caller);
    let slice = data
        .get(ptr as usize..(ptr + len) as usize)
        .ok_or_else(|| anyhow!("Memory read out of bounds"))?;
    Ok(String::from_utf8_lossy(slice).to_string())
}

fn read_wasm_cstr(store: &Store<HostState>, memory: &wasmtime::Memory, ptr: usize) -> anyhow::Result<String> {
    let data  = memory.data(store);
    let start = data.get(ptr..).ok_or_else(|| anyhow!("Ptr out of bounds"))?;
    let end   = start.iter().position(|&b| b == 0).unwrap_or(start.len());
    Ok(String::from_utf8_lossy(&start[..end]).to_string())
}

fn write_wasm_bytes(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> anyhow::Result<i32> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| anyhow!("No memory export"))?;
    let offset = 65536usize;
    memory.write(caller, offset, bytes)?;
    Ok(offset as i32)
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

fn do_http_get(
    client: &reqwest::blocking::Client,
    url: &str,
    headers_json: &str,
) -> anyhow::Result<Vec<u8>> {
    let headers: Value = serde_json::from_str(headers_json).unwrap_or(Value::Null);
    let mut req = client.get(url);
    if let Some(obj) = headers.as_object() {
        for (k, v) in obj {
            if let Some(val) = v.as_str() {
                req = req.header(k.as_str(), val);
            }
        }
    }
    let resp = req.send()?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {}", resp.status(), url);
    }
    Ok(resp.bytes()?.to_vec())
}

// ── UI helpers ────────────────────────────────────────────────────────────────

pub fn describe_action(skill: &str, args: &Value) -> String {
    // Load from manifest, fall back to generic
    if let Ok(dir)      = skill_dir(skill) {
        if let Ok(manifest) = load_manifest(&dir) {
            if let Some(template) = manifest.display.action {
                return render_template(&template, args);
            }
        }
    }
    format!("Running skill: {}", skill)
}

/// Replace {key} placeholders in template with values from args
fn render_template(template: &str, args: &Value) -> String {
    let mut result = template.to_string();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            let placeholder = format!("{{{}}}", k);
            let value = v.as_str().unwrap_or("...");
            result = result.replace(&placeholder, value);
        }
    }
    result
}