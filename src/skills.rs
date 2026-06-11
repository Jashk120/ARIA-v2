//! Host-side skill dispatcher.
//! Loads WASM skill binaries, wires host functions, executes them.
//! Skills are fully self-describing via manifest.toml — core never hardcodes skill logic.

use anyhow::{anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use wasmtime::{Caller, Engine, Linker, Module, Store};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

use crate::db::Db;

// ── Manifest ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct SkillManifest {
    pub name:         String,
    pub version:      String,
    pub description:  String,
    #[serde(default)]
    pub display:      DisplayConfig,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub config:       HashMap<String, ConfigEntry>,
}

#[derive(Deserialize, Default, Debug)]
pub struct DisplayConfig {
    /// Template string, e.g. "Searching the web for \"{query}\""
    /// {key} is replaced with args[key] at runtime
    pub action: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
pub struct Capabilities {
    #[serde(default)]
    pub http: bool,
}

#[derive(Deserialize, Debug)]
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

fn load_manifest(skill_dir: &Path) -> anyhow::Result<SkillManifest> {
    let path = skill_dir.join("manifest.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("Missing manifest.toml at {}", path.display()))?;
    toml::from_str(&text).map_err(|e| anyhow!("Invalid manifest.toml: {}", e))
}

// ── Path resolution ───────────────────────────────────────────────────────────

/// Resolve the root directory of the daemon crate.
fn get_daemon_root() -> anyhow::Result<PathBuf> {
    if let Ok(val) = std::env::var("ARIA_DAEMON_ROOT") {
        return Ok(PathBuf::from(val));
    }
    let exe = std::env::current_exe()?;
    let mut curr = exe.as_path();
    
    // Climb up from target/debug/aria or similar to find daemon/
    while let Some(parent) = curr.parent() {
        if parent.join("Cargo.toml").exists() && parent.join("src").exists() {
            return Ok(parent.to_path_buf());
        }
        curr = parent;
        if curr.as_os_str().is_empty() || curr.parent().is_none() { break; }
    }
    
    // Fallback: one level up from wherever we are
    exe.parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow!("Could not resolve daemon root. Set ARIA_DAEMON_ROOT."))
}

fn skill_dir(name: &str) -> anyhow::Result<PathBuf> {
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    if parts.len() != 2 {
        bail!("Invalid skill name '{}' — expected format: action.category", name);
    }
    let (action, category) = (parts[0], parts[1]);
    let root = get_daemon_root()?;

    Ok(root
        .join("skills")
        .join(category)
        .join(format!("{}.{}", action, category)))
}

fn wasm_path(name: &str) -> anyhow::Result<PathBuf> {
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    if parts.len() != 2 {
        bail!("Invalid skill name '{}' — expected format: action.category", name);
    }
    let (action, category) = (parts[0], parts[1]);
    let root = get_daemon_root()?;

    Ok(root
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join(format!("{}_{}.wasm", action, category)))
}

// ── Skill Manager ─────────────────────────────────────────────────────────────

pub struct SkillManager {
    engine:  Engine,
    modules: RwLock<HashMap<String, Arc<Module>>>,
}

impl SkillManager {
    pub fn new() -> anyhow::Result<Self> {
        let mut config = wasmtime::Config::new();
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
        let args_json = args.to_string();
        let module    = self.get_module(name, &path)?;
        let engine    = self.engine.clone();

        let result = tokio::task::spawn_blocking(move || {
            run_wasm_instance(&engine, module, &args_json, &manifest)
        }).await??;

        Ok(result)
    }
}

// ── Host state ────────────────────────────────────────────────────────────────

struct HostState {
    http_client: reqwest::blocking::Client,
    wasi:        WasiP1Ctx,
}

/// Inject config keys into skill args based on manifest.
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

// ── WASM execution ────────────────────────────────────────────────────────────

/// Memory layout convention:
/// Offset 0: Input JSON (args)
/// Host functions (HTTP etc) append their responses to the end of linear memory at runtime.
const INPUT_BUFFER_OFFSET: usize = 0;
const MAX_INPUT_SIZE: usize = 128 * 1024; // 128KB limit for args
const MAX_HTTP_RESPONSE_SIZE: usize = 5 * 1024 * 1024; // 5MB limit

fn run_wasm_instance(
    engine: &Engine,
    module: Arc<Module>,
    args_json: &str,
    manifest: &SkillManifest
) -> anyhow::Result<Value> {
    let wasi  = WasiCtxBuilder::new().build_p1();
    let state = HostState {
        http_client: reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
        wasi,
    };

    let mut store  = Store::new(engine, state);
    let mut linker: Linker<HostState> = Linker::new(engine);

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

                match do_http_get(&caller.data().http_client, &url, &headers_json) {
                    Ok(body) => {
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

    linker.func_wrap("aria", "host_free", |_: Caller<'_, HostState>, _ptr: i32| {
        // No-op for now as we use a fixed buffer, but prevents skill crash
    })?;

    let instance = linker.instantiate(&mut store, &module)?;
    let memory   = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("Skill has no exported memory"))?;

    // Write input args
    let args_bytes = args_json.as_bytes();
    if args_bytes.len() > MAX_INPUT_SIZE {
        bail!("Input arguments too large (max 128KB)");
    }
    memory.write(&mut store, INPUT_BUFFER_OFFSET, args_bytes)?;

    let run_fn     = instance.get_typed_func::<(i32, i32), i32>(&mut store, "run")?;
    let result_ptr = run_fn.call(&mut store, (INPUT_BUFFER_OFFSET as i32, args_bytes.len() as i32))?;
    
    if result_ptr == 0 {
        bail!("Skill 'run' returned NULL pointer");
    }

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
        .ok_or_else(|| anyhow!("No memory export found"))?;
    
    // 1. Get the current active size of the module's memory footprint
    let current_memory_size = memory.data_size(&mut *caller);
    
    // 2. We use the current boundary as our safe base offset
    let safe_offset = current_memory_size;
    let total_payload_size = bytes.len() + 1; // +1 for null terminator
    
    // 3. Grow the memory space to encapsulate our new host-injected buffer safely out of bounds
    let pages_needed = (total_payload_size + 65535) / 65536;
    memory.grow(&mut *caller, pages_needed as u64)
        .map_err(|_| anyhow!("Could not grow WASM linear memory space for host response buffer"))?;

    // 4. Write data cleanly out of reach of the internal allocator footprint
    let mut data = bytes.to_vec();
    data.push(0); // C-string null terminator
    memory.write(&mut *caller, safe_offset, &data)?;
    
    Ok(safe_offset as i32)
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
    
    let mut buffer = Vec::new();
    use std::io::Read;
    resp.take(MAX_HTTP_RESPONSE_SIZE as u64).read_to_end(&mut buffer)?;
    
    if buffer.len() >= MAX_HTTP_RESPONSE_SIZE {
        eprintln!("[do_http_get] Response truncated to 5MB");
    }
    
    Ok(buffer)
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