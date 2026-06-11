//! Host-side skill dispatcher.
//! Loads WASM skill binaries, wires host functions, executes them.

use anyhow::{anyhow, bail};
use serde_json::Value;
use std::path::PathBuf;
use wasmtime::{Caller, Engine, Linker, Module, Store};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

use crate::db::Db;

// ── Skill path resolution ─────────────────────────────────────────────────────

fn skill_path(name: &str) -> anyhow::Result<PathBuf> {
    // "search.web" → skills/web/search.web/target/wasm32-wasip1/release/search_web.wasm
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    if parts.len() != 2 {
        bail!("Invalid skill name '{}' — expected format: action.category", name);
    }
    let (action, category) = (parts[0], parts[1]);

    let exe = std::env::current_exe()?;
    let root = exe
        .parent()                          // target/debug or target/release
        .and_then(|p| p.parent())          // target
        .and_then(|p| p.parent())          // workspace root
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
    wasi: WasiP1Ctx,
}
// ── Public entry points ───────────────────────────────────────────────────────

/// Run a skill with db key injection (for callers that have db access).
pub async fn run_skill(name: &str, args: &Value, db: &Db) -> anyhow::Result<Value> {
    let mut args = args.clone();
    enrich_args(name, &mut args, db);
    run_skill_raw(name, &args).await
}

/// Run a skill with pre-enriched args — no db access needed.
/// Used from the async ReAct loop where db is not Send.
pub async fn run_skill_raw(name: &str, args: &Value) -> anyhow::Result<Value> {
    let path = skill_path(name)?;

    if !path.exists() {
        bail!(
            "Skill '{}' not found at {}.\nBuild it with: cargo build --target wasm32-wasip1 --release",
            name,
            path.display()
        );
    }

    let args_json = args.to_string();
    let result = tokio::task::spawn_blocking(move || run_wasm(&path, &args_json)).await??;
    Ok(result)
}

/// Inject db-stored keys into skill args.
fn enrich_args(skill: &str, args: &mut Value, db: &Db) {
    match skill {
        "search.web" => {
            let brave = db.get_config("brave_api_key").ok().flatten().unwrap_or_default();
            let searxng = db.get_config("searxng_url").ok().flatten()
                .unwrap_or_else(|| "http://localhost:8080".to_string());
            args["brave_api_key"] = Value::String(brave);
            args["searxng_url"] = Value::String(searxng);
        }
        _ => {}
    }
}
// ── WASM execution ────────────────────────────────────────────────────────────

fn run_wasm(path: &PathBuf, args_json: &str) -> anyhow::Result<Value> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, path)?;

    let wasi = WasiCtxBuilder::new().build_p1();
    let state = HostState {
        http_client: reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
        wasi,
    };
    let mut store = Store::new(&engine, state);
    let mut linker: Linker<HostState> = Linker::new(&engine);

    p1::add_to_linker_sync(&mut linker, |s| &mut s.wasi)?;
        // host_http_get — only network surface available to skills
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
            
            let response = do_http_get(&caller.data().http_client, &url, &headers_json);
            match response {
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

    // host_free — no-op, host owns memory written back to WASM
    linker.func_wrap("aria", "host_free", |_: Caller<'_, HostState>, _ptr: i32| {})?;

    let instance = linker.instantiate(&mut store, &module)?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("Skill has no exported memory"))?;

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

fn read_wasm_str(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> anyhow::Result<String> {
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

fn read_wasm_cstr(store: &Store<HostState>, memory: &wasmtime::Memory, ptr: usize) -> anyhow::Result<String> {
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
    match skill {
        "search.web" => {
            let q = args["query"].as_str().unwrap_or("...");
            format!("Searching the web for \"{}\"", q)
        }
        "scrape.web" => {
            let url = args["url"].as_str().unwrap_or("page");
            format!("Reading {}", url)
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