//! WASM instance execution: host state, memory marshalling, and host
//! function wiring (HTTP, filesystem) gated by manifest capabilities.

use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use wasmtime::{Caller, Engine, Linker, Module, Store};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::{self, WasiP1Ctx};

use super::fs_sandbox::FsSandbox;
use super::manifest::SkillManifest;

/// Memory layout convention:
/// Offset 0: Input JSON (args)
/// Host functions (HTTP, fs, etc) append their responses to the end of linear memory at runtime.
const INPUT_BUFFER_OFFSET: usize = 0;
const MAX_INPUT_SIZE: usize = 128 * 1024; // 128KB limit for args
const MAX_HTTP_RESPONSE_SIZE: usize = 5 * 1024 * 1024; // 5MB limit
const MAX_FS_READ_SIZE: usize = 5 * 1024 * 1024; // 5MB limit per file read

// ── Host state ────────────────────────────────────────────────────────────────

pub struct HostState {
    http_client: reqwest::Client,
    fs_sandbox: Option<FsSandbox>,
    wasi: WasiP1Ctx,
    payment_vault: Option<Arc<crate::payments::direct::PaymentVault>>,
    x402_vault: Option<Arc<crate::payments::x402_vault::X402PaymentVault>>,
    db: Option<Arc<crate::db::Db>>,
    agent_did: String,
    skill_name: String,
    task_id: Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run_wasm_instance_async(
    engine: &Engine,
    module: Arc<Module>,
    args: &Value,
    manifest: &SkillManifest,
    db: Option<Arc<crate::db::Db>>,
    payment_vault: Option<Arc<crate::payments::direct::PaymentVault>>,
    x402_vault: Option<Arc<crate::payments::x402_vault::X402PaymentVault>>,
    agent_did: String,
    skill_name: String,
    task_id: Option<String>,
) -> anyhow::Result<Value> {
    let wasi = WasiCtxBuilder::new().build_p1();

    let fs_sandbox = if manifest.capabilities.fs {
        Some(FsSandbox::from_args(args)?)
    } else {
        None
    };

    let state = HostState {
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
        fs_sandbox,
        wasi,
        payment_vault,
        x402_vault,
        db,
        agent_did,
        skill_name,
        task_id,
    };

    let mut store = Store::new(engine, state);
    let mut linker: Linker<HostState> = Linker::new(engine);

    p1::add_to_linker_async(&mut linker, |s| &mut s.wasi)?;

    if manifest.capabilities.http {
        wire_http(&mut linker)?;
    }
    if manifest.capabilities.fs {
        wire_fs(&mut linker)?;
    }
    if manifest.capabilities.db_query {
        wire_db_query(&mut linker)?;
    }
    if manifest.capabilities.hedera_pay {
        wire_hedera_pay(&mut linker)?;
    }
    if manifest.capabilities.x402_pay {
        wire_x402_pay(&mut linker)?;
    }

    linker.func_wrap(
        "aria",
        "host_free",
        |_: Caller<'_, HostState>, _ptr: i32| {
            // No-op for now as we use a fixed buffer, but prevents skill crash
        },
    )?;

    let instance = linker.instantiate_async(&mut store, &module).await?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("Skill has no exported memory"))?;

    // Write input args
    let args_json = args.to_string();
    let args_bytes = args_json.as_bytes();
    if args_bytes.len() > MAX_INPUT_SIZE {
        bail!("Input arguments too large (max 128KB)");
    }
    memory.write(&mut store, INPUT_BUFFER_OFFSET, args_bytes)?;

    let run_fn = instance.get_typed_func::<(i32, i32), i64>(&mut store, "run")?;
    let packed_result = run_fn
        .call_async(
            &mut store,
            (INPUT_BUFFER_OFFSET as i32, args_bytes.len() as i32),
        )
        .await?;

    if packed_result == 0 {
        bail!("Skill 'run' returned NULL packed pointer/length");
    }

    let (result_ptr, result_len) = unpack_ptr_len(packed_result);
    let data = memory.data(&store);
    let json_bytes = data
        .get(result_ptr..result_ptr + result_len)
        .ok_or_else(|| {
            anyhow!(
                "Skill result memory out of bounds (ptr: {}, len: {})",
                result_ptr,
                result_len
            )
        })?;

    let result_str = std::str::from_utf8(json_bytes)
        .map_err(|e| anyhow!("Skill returned invalid UTF-8: {}", e))?;

    let result: Value = serde_json::from_str(result_str)
        .map_err(|e| anyhow!("Skill returned invalid JSON: {} — raw: {}", e, result_str))?;

    if let Some(err) = result["error"].as_str() {
        bail!("Skill error: {}", err);
    }

    Ok(result)
}

// ── HTTP capability ───────────────────────────────────────────────────────────

fn wire_http(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    linker.func_wrap_async(
        "aria",
        "host_http_get",
        |mut caller: Caller<'_, HostState>,
         (url_ptr, url_len, headers_ptr, headers_len): (i32, i32, i32, i32)| {
            Box::new(async move {
                let url = match read_wasm_str(&mut caller, url_ptr, url_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_http_get] failed to read url: {}", e);
                        return 0;
                    }
                };
                let headers_json = match read_wasm_str(&mut caller, headers_ptr, headers_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_http_get] failed to read headers: {}", e);
                        return 0;
                    }
                };

                match do_http_get(&caller.data().http_client, &url, &headers_json).await {
                    Ok(body) => write_wasm_bytes(&mut caller, &body).await.unwrap_or(0),
                    Err(e) => {
                        eprintln!("[host_http_get] FAILED: {}", e);
                        0
                    }
                }
            })
        },
    )?;
    Ok(())
}

async fn do_http_get(
    client: &reqwest::Client,
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
    let resp = req.send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {}", resp.status(), url);
    }

    let bytes = resp.bytes().await?;
    let mut buffer = bytes.to_vec();
    if buffer.len() > MAX_HTTP_RESPONSE_SIZE {
        buffer.truncate(MAX_HTTP_RESPONSE_SIZE);
        eprintln!("[do_http_get] Response truncated to 5MB");
    }

    Ok(buffer)
}

// ── Filesystem capability ────────────────────────────────────────────────────
//
// All four host_fs_* functions go through `FsSandbox::resolve`, which enforces
// the fs_root / whitelist-or-blacklist policy described in fs_sandbox.rs.
// Errors are surfaced to the guest as a packed-NULL (0) for *_get-style calls;
// the guest skill is expected to translate that into a JSON {"error": ...}
// result on its own "no data returned" path. host_fs_write returns i32 status.

fn wire_fs(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // host_fs_read(path_ptr, path_len) -> packed(ptr,len) of file bytes, or 0 on error
    linker.func_wrap_async(
        "aria",
        "host_fs_read",
        |mut caller: Caller<'_, HostState>, (path_ptr, path_len): (i32, i32)| {
            Box::new(async move {
                let path = match read_wasm_str(&mut caller, path_ptr, path_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_fs_read] failed to read path arg: {}", e);
                        return 0;
                    }
                };

                let resolved = match resolve_sandboxed(&caller, &path, true) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[host_fs_read] {}", e);
                        return 0;
                    }
                };

                match std::fs::read(&resolved) {
                    Ok(mut bytes) => {
                        if bytes.len() > MAX_FS_READ_SIZE {
                            bytes.truncate(MAX_FS_READ_SIZE);
                        }
                        write_wasm_bytes(&mut caller, &bytes).await.unwrap_or(0)
                    }
                    Err(e) => {
                        eprintln!("[host_fs_read] read failed: {}", e);
                        0
                    }
                }
            })
        },
    )?;

    // host_fs_write(path_ptr, path_len, content_ptr, content_len, mode_ptr, mode_len) -> 1 ok / 0 err
    linker.func_wrap_async(
        "aria",
        "host_fs_write",
        |mut caller: Caller<'_, HostState>,
         (path_ptr, path_len, content_ptr, content_len, mode_ptr, mode_len): (
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
        )| {
            Box::new(async move {
                let path = match read_wasm_str(&mut caller, path_ptr, path_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_fs_write] failed to read path arg: {}", e);
                        return 0;
                    }
                };
                let content = match read_wasm_str(&mut caller, content_ptr, content_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_fs_write] failed to read content arg: {}", e);
                        return 0;
                    }
                };
                let mode = read_wasm_str(&mut caller, mode_ptr, mode_len)
                    .unwrap_or_else(|_| "overwrite".to_string());

                let resolved = match resolve_sandboxed(&caller, &path, false) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[host_fs_write] {}", e);
                        return 0;
                    }
                };

                let result = if mode == "append" {
                    use std::io::Write as _;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&resolved)
                        .and_then(|mut f| f.write_all(content.as_bytes()))
                } else {
                    std::fs::write(&resolved, content.as_bytes())
                };

                match result {
                    Ok(()) => 1,
                    Err(e) => {
                        eprintln!("[host_fs_write] write failed: {}", e);
                        0
                    }
                }
            })
        },
    )?;

    // host_fs_list(path_ptr, path_len) -> packed(ptr,len) of JSON array bytes, or 0 on error
    // Each entry: {"name":"...","is_dir":bool,"size":number}
    linker.func_wrap_async(
        "aria",
        "host_fs_list",
        |mut caller: Caller<'_, HostState>, (path_ptr, path_len): (i32, i32)| {
            Box::new(async move {
                let path = match read_wasm_str(&mut caller, path_ptr, path_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_fs_list] failed to read path arg: {}", e);
                        return 0;
                    }
                };

                let resolved = match resolve_sandboxed(&caller, &path, true) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[host_fs_list] {}", e);
                        return 0;
                    }
                };

                let entries = match list_dir(&resolved) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[host_fs_list] {}", e);
                        return 0;
                    }
                };

                let bytes = serde_json::to_vec(&entries).unwrap_or_default();
                write_wasm_bytes(&mut caller, &bytes).await.unwrap_or(0)
            })
        },
    )?;

    // host_fs_find(path_ptr, path_len, query_ptr, query_len, mode_ptr, mode_len)
    //   -> packed(ptr,len) of JSON array bytes: [{"path":"...","preview":"..."}]
    linker.func_wrap_async(
        "aria",
        "host_fs_find",
        |mut caller: Caller<'_, HostState>,
         (path_ptr, path_len, query_ptr, query_len, mode_ptr, mode_len): (
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
        )| {
            Box::new(async move {
                let path = match read_wasm_str(&mut caller, path_ptr, path_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_fs_find] failed to read path arg: {}", e);
                        return 0;
                    }
                };
                let query = match read_wasm_str(&mut caller, query_ptr, query_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_fs_find] failed to read query arg: {}", e);
                        return 0;
                    }
                };
                let mode = read_wasm_str(&mut caller, mode_ptr, mode_len)
                    .unwrap_or_else(|_| "name".to_string());

                let resolved = match resolve_sandboxed(&caller, &path, true) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[host_fs_find] {}", e);
                        return 0;
                    }
                };

                let sandbox = caller.data().fs_sandbox.clone();
                let matches = match find_matches(&resolved, &query, &mode, sandbox.as_ref()) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[host_fs_find] {}", e);
                        return 0;
                    }
                };

                let bytes = serde_json::to_vec(&matches).unwrap_or_default();
                write_wasm_bytes(&mut caller, &bytes).await.unwrap_or(0)
            })
        },
    )?;

    Ok(())
}

/// Resolve a guest-supplied path through this skill's FsSandbox.
fn resolve_sandboxed(
    caller: &Caller<'_, HostState>,
    path: &str,
    must_exist: bool,
) -> anyhow::Result<PathBuf> {
    caller
        .data()
        .fs_sandbox
        .as_ref()
        .ok_or_else(|| anyhow!("fs capability not enabled for this skill"))?
        .resolve(path, must_exist)
}

fn list_dir(dir: &std::path::Path) -> anyhow::Result<Vec<Value>> {
    if !dir.is_dir() {
        bail!("'{}' is not a directory", dir.display());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        out.push(serde_json::json!({
            "name": entry.file_name().to_string_lossy(),
            "is_dir": meta.is_dir(),
            "size": meta.len(),
        }));
    }
    Ok(out)
}

/// `mode`: "name" matches file/dir names (case-insensitive substring), recursively.
///         "content" greps file contents for `query` (text files only, best-effort).
fn find_matches(
    root: &std::path::Path,
    query: &str,
    mode: &str,
    sandbox: Option<&FsSandbox>,
) -> anyhow::Result<Vec<Value>> {
    let query_lower = query.to_lowercase();
    let mut out = Vec::new();
    const MAX_RESULTS: usize = 50;
    const MAX_DEPTH: usize = 12;

    fn walk(
        dir: &std::path::Path,
        query_lower: &str,
        mode: &str,
        depth: usize,
        out: &mut Vec<Value>,
        max_results: usize,
        max_depth: usize,
        root: &std::path::Path,
        sandbox: Option<&FsSandbox>,
    ) -> anyhow::Result<()> {
        if out.len() >= max_results || depth > max_depth {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            if out.len() >= max_results {
                break;
            }
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_lowercase();

            // Per-entry sandbox check, so blacklisted subdirs are skipped entirely.
            if let Some(sb) = sandbox {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                if sb.resolve(&rel, true).is_err() {
                    continue;
                }
            }

            if path.is_dir() {
                if mode == "name" && name.contains(query_lower) {
                    out.push(serde_json::json!({ "path": path.to_string_lossy(), "preview": "" }));
                }
                walk(
                    &path,
                    query_lower,
                    mode,
                    depth + 1,
                    out,
                    max_results,
                    max_depth,
                    root,
                    sandbox,
                )?;
            } else {
                match mode {
                    "content" => {
                        if let Ok(text) = std::fs::read_to_string(&path) {
                            if let Some(pos) = text.to_lowercase().find(query_lower) {
                                let start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
                                let end = text[pos..]
                                    .find('\n')
                                    .map(|i| pos + i)
                                    .unwrap_or(text.len());
                                let preview = text[start..end]
                                    .trim()
                                    .chars()
                                    .take(200)
                                    .collect::<String>();
                                out.push(serde_json::json!({ "path": path.to_string_lossy(), "preview": preview }));
                            }
                        }
                    }
                    _ => {
                        if name.contains(query_lower) {
                            out.push(serde_json::json!({ "path": path.to_string_lossy(), "preview": "" }));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    walk(
        root,
        &query_lower,
        mode,
        0,
        &mut out,
        MAX_RESULTS,
        MAX_DEPTH,
        root,
        sandbox,
    )?;
    Ok(out)
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

fn unpack_ptr_len(packed: i64) -> (usize, usize) {
    let ptr = (packed >> 32) as usize;
    let len = (packed & 0xFFFFFFFF) as usize;
    (ptr, len)
}

async fn write_wasm_bytes(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> anyhow::Result<i64> {
    // Call the guest's allocator explicitly.
    if let Some(export) = caller.get_export("alloc") {
        if let Some(func) = export.into_func() {
            if let Ok(alloc_fn) = func.typed::<i32, i32>(&mut *caller) {
                let total_len = bytes.len() as i32;
                let allocated_ptr = alloc_fn.call_async(&mut *caller, total_len).await?;

                let memory = caller
                    .get_export("memory")
                    .and_then(|e| e.into_memory())
                    .ok_or_else(|| anyhow!("No memory export found"))?;

                memory.write(&mut *caller, allocated_ptr as usize, bytes)?;

                let packed = ((allocated_ptr as u64) << 32) | (bytes.len() as u64);
                return Ok(packed as i64);
            }
        }
    }

    bail!("Skill missing required 'alloc' export for dynamic host-to-guest FFI data passing.");
}
// ── DB query capability ──────────────────────────────────────────────────────
//
// Generic host_db_query ABI, but the host only ever dispatches to a fixed
// allow-list of named, parameterized queries below. WASM never gets raw SQL —
// adding a new query type later just means adding a match arm here, no ABI
// change and no new capability wiring needed for future query types.

fn wire_db_query(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    linker.func_wrap_async(
        "aria",
        "host_db_query",
        |mut caller: Caller<'_, HostState>,
         (type_ptr, type_len, params_ptr, params_len): (i32, i32, i32, i32)| {
            Box::new(async move {
                let query_type = match read_wasm_str(&mut caller, type_ptr, type_len) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[host_db_query] bad query_type: {}", e); return 0; }
                };
                let params_json = match read_wasm_str(&mut caller, params_ptr, params_len) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[host_db_query] bad params: {}", e); return 0; }
                };

                let db = match caller.data().db.clone() {
                    Some(db) => db,
                    None => { eprintln!("[host_db_query] db_query capability not enabled"); return 0; }
                };

                let result = match dispatch_query(&db, &query_type, &params_json) {
                    Ok(v) => v,
                    Err(e) => json!({ "error": e }),
                };

                let bytes = serde_json::to_vec(&result).unwrap_or_default();
                write_wasm_bytes(&mut caller, &bytes).await.unwrap_or(0)
            })
        },
    )?;
    Ok(())
}

/// Fixed allow-list — the only queries any WASM skill can ever trigger.
/// Add new arms here as new needs come up; never expose raw SQL to the guest.
fn dispatch_query(db: &crate::db::Db, query_type: &str, params_json: &str) -> Result<Value, String> {
    match query_type {
        "payments_recent" => {
            let params: Value = serde_json::from_str(params_json).unwrap_or(json!({}));
            let days = params["days"].as_i64().unwrap_or(7);
            let records = db.list_recent_payments(days).map_err(|e| e.to_string())?;
            Ok(json!({ "payments": records }))
        }
        other => Err(format!("unknown query type: {}", other)),
    }
}
// ── x402 payment capability ──────────────────────────────────────────────────
//
// host_x402_pay(url_ptr, url_len) -> packed(ptr,len) of JSON result, or 0 on error
// Fetches a URL, handles HTTP 402 by paying via X402PaymentVault, retries
// with X-PAYMENT header, and returns the unlocked resource body.

fn wire_x402_pay(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    linker.func_wrap_async(
        "aria",
        "host_x402_pay",
        |mut caller: Caller<'_, HostState>, (url_ptr, url_len): (i32, i32)| {
            Box::new(async move {
                let url = match read_wasm_str(&mut caller, url_ptr, url_len) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[host_x402_pay] failed to read url: {}", e); return 0; }
                };

                // Step 1: GET the URL, expecting a 402 response
                let initial_resp = match caller.data().http_client.get(&url).send().await {
                    Ok(r) => r,
                    Err(e) => { eprintln!("[host_x402_pay] initial GET failed: {}", e); return 0; }
                };

                if initial_resp.status() != reqwest::StatusCode::PAYMENT_REQUIRED {
                    eprintln!("[host_x402_pay] expected 402, got: {}", initial_resp.status());
                    return 0;
                }

                // Step 2: Parse PaymentRequirements from the 402 response
                // READING THE 402 RESPONSE — check headers BEFORE body, not body only:
                #[derive(serde::Deserialize)]
                struct PaymentRequiredBody {
                    accepts: Vec<crate::payments::x402_types::PaymentRequirements>,
                }

                let header_opt = initial_resp
                    .headers()
                    .get("PAYMENT-REQUIRED")
                    .and_then(|v| v.to_str().ok());

                let mut payment_body: Option<PaymentRequiredBody> = None;
                let mut header_parsed = false;

                if let Some(hdr_val) = header_opt {
                    if let Ok(decoded) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, hdr_val) {
                        if let Ok(parsed) = serde_json::from_slice::<PaymentRequiredBody>(&decoded) {
                            payment_body = Some(parsed);
                            header_parsed = true;
                        } else {
                            eprintln!("[host_x402_pay] failed to parse PAYMENT-REQUIRED header JSON");
                        }
                    } else {
                        eprintln!("[host_x402_pay] failed to base64-decode PAYMENT-REQUIRED header");
                    }
                }

                if payment_body.is_none() {
                    // Try parsing the body
                    if let Ok(body_text) = initial_resp.text().await {
                        if !body_text.trim().is_empty() {
                            match serde_json::from_str::<PaymentRequiredBody>(&body_text) {
                                Ok(parsed) => {
                                    payment_body = Some(parsed);
                                }
                                Err(e) => {
                                    if !header_parsed {
                                        eprintln!("[host_x402_pay] failed to parse 402 body: {} (no valid header found)", e);
                                        return 0;
                                    }
                                }
                            }
                        } else if !header_parsed {
                            eprintln!("[host_x402_pay] 402 response body is empty and no valid header found");
                            return 0;
                        }
                    } else if !header_parsed {
                        eprintln!("[host_x402_pay] failed to read 402 body and no valid header found");
                        return 0;
                    }
                }

                let payment_body = match payment_body {
                    Some(b) => b,
                    None => {
                        eprintln!("[host_x402_pay] no valid PaymentRequirements found anywhere");
                        return 0;
                    }
                };

                // Find a hedera:testnet requirement
                let requirements = match payment_body.accepts.into_iter().find(|r| r.network == "hedera:testnet") {
                    Some(r) => r,
                    None => { eprintln!("[host_x402_pay] no hedera:testnet requirement in 402 accepts"); return 0; }
                };

                // Step 3: Pay via X402PaymentVault
                let vault: Arc<crate::payments::x402_vault::X402PaymentVault> =
                    match caller.data().x402_vault.clone() {
                    Some(v) => v,
                    None => { eprintln!("[host_x402_pay] x402_pay capability not enabled"); return 0; }
                };

                let payment_result = match vault.pay(requirements.clone()).await {
                    Ok(r) => r,
                    Err(e) => { eprintln!("[host_x402_pay] payment failed: {}", e); return 0; }
                };

                // Step 5: Retry with PAYMENT-SIGNATURE header
                let retry_resp = match caller.data().http_client
                    .get(&url)
                    .header("PAYMENT-SIGNATURE", &payment_result.payment_token)
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[host_x402_pay] retry GET failed: {}", e);
                        if let Some(db) = caller.data().db.clone() {
                            if let Err(db_err) = db.update_payment_status(&payment_result.transaction_id, "delivery_failed") {
                                 eprintln!("[host_x402_pay] failed to update payment status to delivery_failed: {}", db_err);
                            }
                        }
                        return 0;
                    }
                };

                if !retry_resp.status().is_success() {
                    eprintln!("[host_x402_pay] retry returned non-200: {}", retry_resp.status());
                    if let Some(db) = caller.data().db.clone() {
                        if let Err(db_err) = db.update_payment_status(&payment_result.transaction_id, "delivery_failed") {
                            eprintln!("[host_x402_pay] failed to update payment status to delivery_failed: {}", db_err);
                        }
                    }
                    return 0;
                }

                // READING THE SETTLEMENT CONFIRMATION — check for PAYMENT-RESPONSE header on successful response:
                if let Some(resp_hdr_val) = retry_resp
                    .headers()
                    .get("PAYMENT-RESPONSE")
                    .and_then(|v| v.to_str().ok())
                {
                    if let Ok(decoded) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, resp_hdr_val) {
                        match serde_json::from_slice::<crate::payments::x402_types::SettleResponse>(&decoded) {
                            Ok(settle_resp) => {
                                eprintln!("[host_x402_pay] Successfully parsed PAYMENT-RESPONSE header: {:?}", settle_resp);
                            }
                            Err(e) => {
                                eprintln!("[host_x402_pay] failed to parse PAYMENT-RESPONSE header JSON: {}", e);
                            }
                        }
                    } else {
                        eprintln!("[host_x402_pay] failed to base64-decode PAYMENT-RESPONSE header");
                    }
                }

                // Step 6: Parse response body — JSON if content-type is JSON, else string
                let content_type = retry_resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                let body_bytes = match retry_resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("[host_x402_pay] failed to read retry body: {}", e);
                        if let Some(db) = caller.data().db.clone() {
                            if let Err(db_err) = db.update_payment_status(&payment_result.transaction_id, "delivery_failed") {
                                eprintln!("[host_x402_pay] failed to update payment status to delivery_failed: {}", db_err);
                            }
                        }
                        return 0;
                    }
                };

                let data: serde_json::Value = if content_type.contains("application/json") {
                    match serde_json::from_slice(&body_bytes) {
                        Ok(v) => v,
                        Err(_) => serde_json::Value::String(String::from_utf8_lossy(&body_bytes).to_string()),
                    }
                } else {
                    serde_json::Value::String(String::from_utf8_lossy(&body_bytes).to_string())
                };

                // Step 7: Return JSON matching pay.x402 output_schema
                let result = serde_json::json!({
                    "data": data,
                    "transaction_id": payment_result.transaction_id,
                    "hashscan_url": payment_result.hashscan_url,
                });

                let bytes = serde_json::to_vec(&result).unwrap_or_default();
                write_wasm_bytes(&mut caller, &bytes).await.unwrap_or(0)
            })
        },
    )?;
    Ok(())
}

// ── Hedera payment capability ────────────────────────────────────────────────

fn wire_hedera_pay(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    linker.func_wrap_async(
        "aria",
        "host_hedera_pay",
        |mut caller: Caller<'_, HostState>,
         (recipient_ptr, recipient_len, amount_ptr, amount_len, memo_ptr, memo_len): (
            i32, i32, i32, i32, i32, i32,
        )| {
            Box::new(async move {
                let recipient = match read_wasm_str(&mut caller, recipient_ptr, recipient_len) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[host_hedera_pay] bad recipient: {}", e); return 0; }
                };
                let amount_str = match read_wasm_str(&mut caller, amount_ptr, amount_len) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[host_hedera_pay] bad amount: {}", e); return 0; }
                };
                let memo = match read_wasm_str(&mut caller, memo_ptr, memo_len) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[host_hedera_pay] bad memo: {}", e); return 0; }
                };
                let amount: f64 = match amount_str.parse() {
                    Ok(a) => a,
                    Err(e) => { eprintln!("[host_hedera_pay] bad amount format: {}", e); return 0; }
                };

                let vault = match caller.data().payment_vault.clone() {
                    Some(v) => v,
                    None => { eprintln!("[host_hedera_pay] hedera_pay capability not enabled"); return 0; }
                };

                let receipt = match vault.pay(&recipient, amount, &memo).await {
                    Ok(r) => r,
                    Err(e) => { eprintln!("[host_hedera_pay] payment failed: {}", e); return 0; }
                };

                // Log to db right here — host is the only side with db access.
                // agent_did/skill_called/task_id are best-effort context that
                // HostState should carry in from run_skill_raw (see note below).
                if let Some(db) = caller.data().db.clone() {
                    let state = caller.data();
                    if let Err(e) = db.insert_payment(
                        state.task_id.as_deref(),
                        &state.agent_did,
                        &state.skill_name,
                        &recipient,
                        amount,
                        &memo,
                        &receipt.transaction_id,
                        &receipt.hashscan_url,
                        &receipt.status,
                    ) {
                        eprintln!("[host_hedera_pay] failed to log payment to db: {}", e);
                        // Don't fail the payment over a logging error — the
                        // money already moved on-chain; the receipt is still returned.
                    }
                }

                let bytes = serde_json::to_vec(&json!({
                    "transaction_id": receipt.transaction_id,
                    "hashscan_url": receipt.hashscan_url,
                    "status": receipt.status,
                })).unwrap_or_default();
                write_wasm_bytes(&mut caller, &bytes).await.unwrap_or(0)
            })
        },
    )?;
    Ok(())
}
