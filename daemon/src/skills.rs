//! Host-side skill dispatcher.
//! Loads WASM skill binaries, wires host functions, executes them.
//! repl.rs calls `run_skill(name, args, db)` — never touches wasmtime directly.

use anyhow::{anyhow, bail};
use serde_json::Value;
use std::path::PathBuf;
use wasmtime::{Caller, Engine, Linker, Module, Store};

use crate::db::Db;

// ── Skill path resolution ─────────────────────────────────────────────────────

fn skill_path(name: &str) -> anyhow::Result<PathBuf> {
    // "search.web" → skills/web/search.web/search.web.wasm
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    if parts.len() != 2 {
        bail!("Invalid skill name '{}' — expected format: action.category", name);
    }
    let (action, category) = (parts[0], parts[1]);

    let exe = std::env::current_exe()?;
    let root = exe
        .parent()                          // target/release
        .and_then(|p| p.parent())          // target
        .and_then(|p| p.parent())          // workspace root
        .ok_or_else(|| anyhow!("Could not resolve workspace root"))?;

    Ok(root
        .join("skills")
        .join(category)
        .join(format!("{}.{}", action, category))
        .join(format!("{}.{}.wasm", action, category)))
}

// ── Host state ────────────────────────────────────────────────────────────────

struct HostState {
    http_client: reqwest::blocking::Client,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Run a skill. Looks up optional keys from db and injects them into args.
pub async fn run_skill(name: &str, args: &Value, db: &Db) -> anyhow::Result<Value> {
    let path = skill_path(name)?;

    if !path.exists() {
        bail!(
            "Skill '{}' not found at {}.\nRun: cargo build --target wasm32-wasip1 --release",
            name,
            path.display()
        );
    }

    // Inject optional keys into args so the skill can use them
    let mut args = args.clone();
    enrich_args(name, &mut args, db);

    let name = name.to_string();
    let result = tokio::task::spawn_blocking(move || {
        run_wasm(&path, &args.to_string())
    })
    .await??;

    Ok(result)
}

/// Inject db-stored keys into skill args before execution.
/// The skill receives them as normal input fields — no special treatment needed.
fn enrich_args(skill: &str, args: &mut Value, db: &Db) {
    match skill {
        "search.web" => {
            // Inject Brave key if configured — skill falls back to DDG if empty
            let key = db.get_config("brave_api_key")
                .ok()
                .flatten()
                .unwrap_or_default();
            args["brave_api_key"] = Value::String(key);
        }
        _ => {}
    }
}

// ── WASM execution ────────────────────────────────────────────────────────────

fn run_wasm(path: &PathBuf, args_json: &str) -> anyhow::Result<Value> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, path)?;

    let state = HostState {
        http_client: reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
    };
    let mut store = Store::new(&engine, state);
    let mut linker: Linker<HostState> = Linker::new(&engine);

    // host_http_get — the only network surface the skill has access to
    linker.func_wrap(
        "aria",
        "host_http_get",
        |mut caller: Caller<'_, HostState>,
         url_ptr: i32, url_len: i32,
         headers_ptr: i32, headers_len: i32|
         -> i32 {
            let url = match read_wasm_str(&caller, url_ptr, url_len) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            let headers_json = match read_wasm_str(&caller, headers_ptr, headers_len) {
                Ok(s) => s,
                Err(_) => return 0,
            };

            let response = do_http_get(&caller.data().http_client, &url, &headers_json);

            match response {
                Ok(body) => write_wasm_bytes(&mut caller, &body).unwrap_or(0),
                Err(_)   => 0,
            }
        },
    )?;

    // host_free — no-op, host owns the memory written back to WASM
    linker.func_wrap("aria", "host_free", |_: Caller<'_, HostState>, _ptr: i32| {})?;

    let instance = linker.instantiate(&mut store, &module)?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("Skill has no exported memory"))?;

    // Write args into WASM linear memory at offset 0
    let args_bytes = args_json.as_bytes();
    memory.write(&mut store, 0, args_bytes)?;

    let run_fn = instance.get_typed_func::<(i32, i32), i32>(&mut store, "run")?;
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

fn read_wasm_str(caller: &Caller<'_, HostState>, ptr: i32, len: i32) -> anyhow::Result<String> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| anyhow!("No memory export"))?;
    let data = memory.data(caller);
    let slice = data
        .get(ptr as usize..(ptr + len) as usize)
        .ok_or_else(|| anyhow!("Memory read out of bounds"))?;
    Ok(String::from_utf8_lossy(slice).to_string())
}

fn read_wasm_cstr(
    store: &Store<HostState>,
    memory: &wasmtime::Memory,
    ptr: usize,
) -> anyhow::Result<String> {
    let data = memory.data(store);
    let start = data.get(ptr..).ok_or_else(|| anyhow!("Ptr out of bounds"))?;
    let end = start.iter().position(|&b| b == 0).unwrap_or(start.len());
    Ok(String::from_utf8_lossy(&start[..end]).to_string())
}

fn write_wasm_bytes(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> anyhow::Result<i32> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| anyhow!("No memory export"))?;
    // Write at high offset to avoid clobbering skill's own stack/heap
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

// ── UI helpers (used by repl.rs) ──────────────────────────────────────────────

pub fn describe_action(skill: &str, args: &Value) -> String {
    match skill {
        "search.web" => {
            let q = args["query"].as_str().unwrap_or("...");
            format!("Searching the web for \"{}\"", q)
        }
        "scrape.web" => {
            let url = args["url"].as_str().unwrap_or("page");
            format!("Reading {}", url)
        }
        "reddit.web" => {
            let q = args["query"].as_str().unwrap_or("...");
            format!("Searching Reddit for \"{}\"", q)
        }
        "read.fs" => {
            let path = args["path"].as_str().unwrap_or("file");
            format!("Reading {}", path)
        }
        "find.fs" => {
            let path    = args["path"].as_str().unwrap_or("~");
            let pattern = args["pattern"].as_str().unwrap_or("*");
            format!("Searching {} for {}", path, pattern)
        }
        other => format!("Running skill: {}", other),
    }
}
