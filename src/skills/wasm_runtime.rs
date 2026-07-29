//! WASM instance execution: host state, memory marshalling, and host
//! function wiring (HTTP, filesystem) gated by manifest capabilities.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{
    anyhow,
    bail,
};
use serde_json::{
    Value,
    json,
};
use wasmtime::{
    Caller,
    Engine,
    Linker,
    Module,
    Store,
};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::{
    self,
    WasiP1Ctx,
};

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

    let fs_sandbox =
        if manifest.capabilities.fs { Some(FsSandbox::from_args(args)?) } else { None };

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

    linker.func_wrap("aria", "host_free", |_: Caller<'_, HostState>, _ptr: i32| {
        // No-op for now as we use a fixed buffer, but prevents skill crash
    })?;

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
        .call_async(&mut store, (INPUT_BUFFER_OFFSET as i32, args_bytes.len() as i32))
        .await?;

    if packed_result == 0 {
        bail!("Skill 'run' returned NULL packed pointer/length");
    }

    let (result_ptr, result_len) = unpack_ptr_len(packed_result);
    let data = memory.data(&store);
    let json_bytes = data.get(result_ptr..result_ptr + result_len).ok_or_else(|| {
        anyhow!("Skill result memory out of bounds (ptr: {}, len: {})", result_ptr, result_len)
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
//
// Packed-return sentinel values for host_http_get (closure return type: i64):
//   0     = generic error (network failure, non-2xx non-402, parse error)
//   -1i64 = HTTP 402 Payment Required detected. The host closure type is i64,
//           so we use -1i64 (bit pattern 0xFFFFFFFFFFFFFFFF). The guest
//           declares the import as `-> u64` and WASM reinterprets the bits,
//           so the guest sees u64::MAX and can check `packed == u64::MAX`.
//           Guest must NOT treat this as a generic failure — it should return
//           {"error":"payment_required:url=<url>"} so the LLM can act on it.
//   other = (ptr << 32 | len): points to the response body in guest linear
//           memory (the normal success path, ptr and len both > 0 and < 2^32).
//
// -1i64 is safe as a sentinel: a valid allocation packed value has both
// ptr > 0 and len > 0, and (ptr << 32 | len) == 0xFFFFFFFFFFFFFFFF only if
// ptr == 0xFFFFFFFF AND len == 0xFFFFFFFF simultaneously, which would require
// ~8 GB of WASM linear memory — impossible in practice.

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

                match do_fetch_with_402_detection(&caller.data().http_client, &url, &headers_json).await {
                    FetchResult::Success(body) => {
                        write_wasm_bytes(&mut caller, &body).await.unwrap_or(0)
                    }
                    FetchResult::PaymentRequired(_req) => {
                        // Signal the guest that this URL requires payment.
                        // The guest must return {"error":"payment_required:url=..."}.
                        // We return -1i64 (bit pattern 0xFFFFFFFFFFFFFFFF) because the
                        // closure type is i64. The guest declares the import as -> u64
                        // and WASM reinterprets the bits as u64::MAX, which is our sentinel.
                        // See sentinel comment above for why this value is safe.
                        eprintln!("[host_http_get] 402 Payment Required for {}", url);
                        -1i64
                    }
                    FetchResult::PaymentRequiredUnparseable => {
                        eprintln!("[host_http_get] 402 received but PaymentRequirements unparseable for {}", url);
                        0
                    }
                    FetchResult::HttpError(status) => {
                        eprintln!("[host_http_get] HTTP {} for {}", status, url);
                        0
                    }
                    FetchResult::NetworkError(e) => {
                        eprintln!("[host_http_get] FAILED: {}", e);
                        0
                    }
                }
            })
        },
    )?;
    Ok(())
}

// ── Shared fetch + 402-detection primitive ────────────────────────────────────
//
// Used by both wire_http (scrape.web path) and wire_x402_pay (pay.x402 path).
// This is the single source of truth for GET + 402-header-first-then-body parsing.

#[derive(serde::Deserialize)]
struct PaymentRequiredBody {
    accepts: Vec<crate::payments::x402_types::PaymentRequirements>,
}

enum FetchResult {
    /// 2xx response; body bytes (truncated to MAX_HTTP_RESPONSE_SIZE).
    Success(Vec<u8>),
    /// HTTP 402 and we successfully parsed a hedera:testnet PaymentRequirements.
    PaymentRequired(crate::payments::x402_types::PaymentRequirements),
    /// HTTP 402 but we could not parse PaymentRequirements (or no hedera:testnet entry).
    PaymentRequiredUnparseable,
    /// Non-2xx, non-402 status.
    HttpError(reqwest::StatusCode),
    /// Network / send error.
    NetworkError(String),
}

async fn do_fetch_with_402_detection(
    client: &reqwest::Client,
    url: &str,
    headers_json: &str,
) -> FetchResult {
    // Build request with caller-supplied headers.
    let headers: Value = serde_json::from_str(headers_json).unwrap_or(Value::Null);
    let mut req = client.get(url);
    if let Some(obj) = headers.as_object() {
        for (k, v) in obj {
            if let Some(val) = v.as_str() {
                req = req.header(k.as_str(), val);
            }
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return FetchResult::NetworkError(e.to_string()),
    };

    let status = resp.status();

    if status.is_success() {
        // Normal success path — return body, respecting size limit.
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return FetchResult::NetworkError(e.to_string()),
        };
        let mut buffer = bytes.to_vec();
        if buffer.len() > MAX_HTTP_RESPONSE_SIZE {
            buffer.truncate(MAX_HTTP_RESPONSE_SIZE);
            eprintln!("[do_fetch_with_402_detection] Response truncated to 5MB for {}", url);
        }
        return FetchResult::Success(buffer);
    }

    if status != reqwest::StatusCode::PAYMENT_REQUIRED {
        return FetchResult::HttpError(status);
    }

    // ── 402 path: parse PaymentRequirements ───────────────────────────────────
    //
    // Protocol: check PAYMENT-REQUIRED header first (base64-encoded JSON
    // `{accepts:[...]}` per the x402 wire spec); fall back to parsing the
    // response body directly if the header is absent or unparseable.

    let header_opt =
        resp.headers().get("PAYMENT-REQUIRED").and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    let mut payment_body: Option<PaymentRequiredBody> = None;
    let mut header_parsed = false;

    if let Some(ref hdr_val) = header_opt {
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, hdr_val) {
            Ok(decoded) => match serde_json::from_slice::<PaymentRequiredBody>(&decoded) {
                Ok(parsed) => {
                    payment_body = Some(parsed);
                    header_parsed = true;
                }
                Err(e) => {
                    eprintln!(
                        "[do_fetch_with_402_detection] failed to parse PAYMENT-REQUIRED header JSON: {}",
                        e
                    );
                }
            },
            Err(e) => {
                eprintln!(
                    "[do_fetch_with_402_detection] failed to base64-decode PAYMENT-REQUIRED header: {}",
                    e
                );
            }
        }
    }

    if payment_body.is_none() {
        // Fallback: try to parse the body as `{accepts:[...]}`.
        match resp.text().await {
            Ok(body_text) => {
                if !body_text.trim().is_empty() {
                    match serde_json::from_str::<PaymentRequiredBody>(&body_text) {
                        Ok(parsed) => {
                            payment_body = Some(parsed);
                        }
                        Err(e) => {
                            if !header_parsed {
                                eprintln!(
                                    "[do_fetch_with_402_detection] failed to parse 402 body: {} (no valid header found)",
                                    e
                                );
                            }
                        }
                    }
                } else if !header_parsed {
                    eprintln!(
                        "[do_fetch_with_402_detection] 402 response body is empty and no valid header found"
                    );
                }
            }
            Err(e) => {
                if !header_parsed {
                    eprintln!("[do_fetch_with_402_detection] failed to read 402 body: {}", e);
                }
            }
        }
    }

    // Find the hedera:testnet entry from the accepts list (same selection logic
    // as the original wire_x402_pay).
    match payment_body {
        Some(body) => match body.accepts.into_iter().find(|r| r.network == "hedera:testnet") {
            Some(requirements) => FetchResult::PaymentRequired(requirements),
            None => {
                eprintln!(
                    "[do_fetch_with_402_detection] no hedera:testnet requirement in 402 accepts"
                );
                FetchResult::PaymentRequiredUnparseable
            }
        },
        None => {
            eprintln!(
                "[do_fetch_with_402_detection] no valid PaymentRequirements found in 402 response"
            );
            FetchResult::PaymentRequiredUnparseable
        }
    }
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
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
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
                        if let Ok(text) = std::fs::read_to_string(&path)
                            && let Some(pos) = text.to_lowercase().find(query_lower)
                        {
                            let start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
                            let end = text[pos..].find('\n').map(|i| pos + i).unwrap_or(text.len());
                            let preview =
                                text[start..end].trim().chars().take(200).collect::<String>();
                            out.push(serde_json::json!({ "path": path.to_string_lossy(), "preview": preview }));
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

    walk(root, &query_lower, mode, 0, &mut out, MAX_RESULTS, MAX_DEPTH, root, sandbox)?;
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
    if let Some(export) = caller.get_export("alloc")
        && let Some(func) = export.into_func()
        && let Ok(alloc_fn) = func.typed::<i32, i32>(&mut *caller)
    {
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
                    Err(e) => {
                        eprintln!("[host_db_query] bad query_type: {}", e);
                        return 0;
                    }
                };
                let params_json = match read_wasm_str(&mut caller, params_ptr, params_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_db_query] bad params: {}", e);
                        return 0;
                    }
                };

                let db = match caller.data().db.clone() {
                    Some(db) => db,
                    None => {
                        eprintln!("[host_db_query] db_query capability not enabled");
                        return 0;
                    }
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
fn dispatch_query(
    db: &crate::db::Db,
    query_type: &str,
    params_json: &str,
) -> Result<Value, String> {
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
// host_x402_pay(url_ptr, url_len) -> packed(ptr,len) of JSON result
// Fetches a URL via do_fetch_with_402_detection. If the URL requires payment
// (402), pays via X402PaymentVault and retries with PAYMENT-SIGNATURE header.
// If the URL is freely accessible (2xx), returns the data directly without payment.
//
// On any error or policy block, writes a JSON {"error":"<reason>"} into WASM
// memory and returns its packed ptr — the guest skill surfaces this to the LLM
// as a structured error instead of the opaque "host call failed" that a 0 return
// would produce.

/// Write a JSON `{"error": "<msg>"}` into WASM guest memory and return the
/// packed (ptr << 32 | len) result, or 0 if writing fails.
async fn write_wasm_error(caller: &mut Caller<'_, HostState>, msg: &str) -> i64 {
    let json = serde_json::json!({ "error": msg });
    let bytes = serde_json::to_vec(&json).unwrap_or_default();
    write_wasm_bytes(caller, &bytes).await.unwrap_or(0)
}

fn wire_x402_pay(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    linker.func_wrap_async(
        "aria",
        "host_x402_pay",
        |mut caller: Caller<'_, HostState>, (url_ptr, url_len): (i32, i32)| {
            Box::new(async move {
                let url = match read_wasm_str(&mut caller, url_ptr, url_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_x402_pay] failed to read url: {}", e);
                        return write_wasm_error(&mut caller, &format!("x402.pay: could not read url argument: {}", e)).await;
                    }
                };

                // Step 1: GET the URL using the shared fetch+402-detection primitive.
                // Pass empty headers for the initial probe (no auth yet).
                let requirements = match do_fetch_with_402_detection(
                    &caller.data().http_client, &url, "{}"
                ).await {
                    FetchResult::Success(body) => {
                        // URL is freely accessible — no payment needed. Return as-is.
                        eprintln!("[host_x402_pay] URL did not require payment (2xx), returning body directly");
                        let body_str = String::from_utf8_lossy(&body).to_string();
                        let data: serde_json::Value = serde_json::from_slice(&body)
                            .unwrap_or(serde_json::Value::String(body_str));
                        let result = serde_json::json!({
                            "data": data,
                            "transaction_id": serde_json::Value::Null,
                            "hashscan_url": serde_json::Value::Null,
                        });
                        let bytes = serde_json::to_vec(&result).unwrap_or_default();
                        return write_wasm_bytes(&mut caller, &bytes).await.unwrap_or(0);
                    }
                    FetchResult::PaymentRequired(mut req) => {
                        if req.extra.get("url").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                            if let Some(obj) = req.extra.as_object_mut() {
                                obj.insert("url".to_string(), serde_json::Value::String(url.clone()));
                            } else {
                                let mut obj = serde_json::Map::new();
                                obj.insert("url".to_string(), serde_json::Value::String(url.clone()));
                                req.extra = serde_json::Value::Object(obj);
                            }
                        }
                        req
                    }

                    FetchResult::PaymentRequiredUnparseable => {
                        eprintln!("[host_x402_pay] 402 received but PaymentRequirements unparseable for {}", url);
                        return write_wasm_error(
                            &mut caller,
                            &format!("x402.pay: server at '{}' returned HTTP 402 but the payment requirements could not be parsed — the server may not support the x402/Hedera protocol", url)
                        ).await;
                    }
                    FetchResult::HttpError(status) => {
                        eprintln!("[host_x402_pay] initial GET returned non-402 error: {} for {}", status, url);
                        return write_wasm_error(
                            &mut caller,
                            &format!("x402.pay: HTTP {} from '{}' — not a payment-gated resource", status, url)
                        ).await;
                    }
                    FetchResult::NetworkError(e) => {
                        eprintln!("[host_x402_pay] initial GET failed: {}", e);
                        return write_wasm_error(
                            &mut caller,
                            &format!("x402.pay: network error reaching '{}': {}", url, e)
                        ).await;
                    }
                };

                // Step 1.5: Governance checks (allowlist, per-task cap, per-day cap,
                // rate limit). x402 never asks a human to confirm — this whole check
                // runs autonomously — so everything hedera_pay does at proposal time
                // (react_loop.rs) has to happen here instead, now that pay_to/amount
                // are finally known from `requirements`.
                let pay_to = requirements.pay_to.clone();
                // Same tinybar-to-HBAR conversion as x402_vault.rs's own bookkeeping.
                // Do not reintroduce a magnitude-based unit heuristic here.
                let amount_hbar: f64 =
                    requirements.amount.parse::<f64>().unwrap_or(0.0) / 100_000_000.0;

                let db = match caller.data().db.clone() {
                    Some(d) => d,
                    None => {
                        eprintln!("[host_x402_pay] db not available; cannot run payment governance checks");
                        return write_wasm_error(&mut caller, "x402.pay: internal error — database not available for governance checks").await;
                    }
                };
                let agent_did = caller.data().agent_did.clone();

                // Resolve HCS audit client + topic once, used across all governance checks below.
                // Mirrors what react_loop.rs does for hedera_pay.
                let audit_client = caller.data().x402_vault.as_ref().map(|v| v.client());
                let topic_id = crate::config::RuntimeConfig::load(&db).governance.audit_topic_id.clone();
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                // Log the attempt unconditionally, before any allow/block decision,
                // so a blocked spam loop still counts toward its own rate limit.
                // Keyed on the request url, not pay_to — one provider can serve
                // multiple distinct resources from the same payout account, so
                // account-keyed rate limiting/allowlisting conflated unrelated
                // resources into one bucket. This intentionally means a url
                // that's reconfigured or MITM'd to redirect payment to a
                // different account is no longer caught by this check.
                if let Err(e) = db.log_url_payment_attempt(&agent_did, &url) {
                    eprintln!("[host_x402_pay] failed to log payment attempt: {}", e);
                }

                let recent_attempts =
                    db.count_recent_url_attempts(&agent_did, &url).unwrap_or(0);
                if recent_attempts > 10 {
                    eprintln!(
                        "[host_x402_pay] blocked by rate limit: {} attempts to '{}' in the last hour (max 10)",
                        recent_attempts, url
                    );
                    crate::payments::audit::write_payment_decision(
                        audit_client.clone(), topic_id.clone(),
                        crate::payments::audit::CurbRecord {
                            v: 1, agent: agent_did.clone(), ts: now_ms,
                            policy: Some("curb.rate-limit".to_string()),
                            method: Some("x402.pay".to_string()),
                            amount: Some(0.0),
                            currency: Some("HBAR".to_string()),
                            counterparty: Some(url.clone()),
                            allowed: Some(false),
                            reason: Some(format!("rate_limit_exceeded:{}", recent_attempts)),
                            request_id: None,
                        },
                    );
                    return write_wasm_error(
                        &mut caller,
                        &format!(
                            "x402.pay blocked by rate limit: {} payment attempts to '{}' in the last hour (max 10). Wait before retrying.",
                            recent_attempts, url
                        )
                    ).await;
                }

                let is_allowed = db.is_url_allowlisted(&agent_did, &url).unwrap_or(false);
                crate::payments::audit::write_payment_decision(
                    audit_client.clone(), topic_id.clone(),
                    crate::payments::audit::CurbRecord {
                        v: 1, agent: agent_did.clone(), ts: now_ms,
                        policy: Some("curb.allowlist".to_string()),
                        method: Some("x402.pay".to_string()),
                        amount: Some(amount_hbar),
                        currency: Some("HBAR".to_string()),
                        counterparty: Some(url.clone()),
                        allowed: Some(is_allowed),
                        reason: Some(if is_allowed {
                            "allowlisted".to_string()
                        } else {
                            format!("not_allowlisted:{}", url)
                        }),
                        request_id: None,
                    },
                );
                if !is_allowed {
                    eprintln!(
                        "[host_x402_pay] blocked by policy (curb.allowlist): url '{}' is not on the allowlist (pay_to='{}', amount={} HBAR)",
                        url, pay_to, amount_hbar
                    );
                    return write_wasm_error(
                        &mut caller,
                        &format!(
                            "x402.pay blocked by policy (curb.url-allowlist): '{}' is not on the approved URL allowlist. \
                             Add it in Settings → URL Allowlist before paying.",
                            url
                        )
                    ).await;
                }

                let runtime_cfg = crate::config::RuntimeConfig::load(&db);
                let governance = &runtime_cfg.governance;

                if let Some(per_task) = governance.per_task_cap {
                    if amount_hbar > per_task {
                        eprintln!(
                            "[host_x402_pay] blocked by policy (curb.spend-limit): amount {} HBAR exceeds per-task cap of {} HBAR",
                            amount_hbar, per_task
                        );
                        crate::payments::audit::write_payment_decision(
                            audit_client.clone(), topic_id.clone(),
                            crate::payments::audit::CurbRecord {
                                v: 1, agent: agent_did.clone(), ts: now_ms,
                                policy: Some("curb.spend-limit".to_string()),
                                method: Some("x402.pay".to_string()),
                                amount: Some(amount_hbar),
                                currency: Some("HBAR".to_string()),
                                counterparty: Some(pay_to.clone()),
                                allowed: Some(false),
                                reason: Some("per_task_exceeded".to_string()),
                                request_id: None,
                            },
                        );
                        return write_wasm_error(
                            &mut caller,
                            &format!(
                                "x402.pay blocked by policy (curb.spend-limit): {:.6} HBAR exceeds the per-task cap of {:.6} HBAR",
                                amount_hbar, per_task
                            )
                        ).await;
                    }
                }

                let pkey = crate::payments::governance::compute_payment_key(&agent_did, &pay_to, amount_hbar);
                let reserved = db
                    .try_reserve_spend(&agent_did, &pkey, amount_hbar, governance.per_day_cap)
                    .unwrap_or(false);
                crate::payments::audit::write_payment_decision(
                    audit_client.clone(), topic_id.clone(),
                    crate::payments::audit::CurbRecord {
                        v: 1, agent: agent_did.clone(), ts: now_ms,
                        policy: Some("curb.spend-limit".to_string()),
                        method: Some("x402.pay".to_string()),
                        amount: Some(amount_hbar),
                        currency: Some("HBAR".to_string()),
                        counterparty: Some(pay_to.clone()),
                        allowed: Some(reserved),
                        reason: Some(if reserved {
                            "within_budget".to_string()
                        } else {
                            "per_day_exceeded".to_string()
                        }),
                        request_id: None,
                    },
                );
                if !reserved {
                    eprintln!(
                        "[host_x402_pay] blocked by policy (curb.spend-limit): payment of {} HBAR exceeds rolling 24-hour daily budget cap",
                        amount_hbar
                    );
                    return write_wasm_error(
                        &mut caller,
                        &format!(
                            "x402.pay blocked by policy (curb.spend-limit): {:.6} HBAR exceeds the rolling 24-hour daily budget cap",
                            amount_hbar
                        )
                    ).await;
                }

                // x402 is always auto-approved (no human confirmation step) — log it.
                crate::payments::audit::write_payment_decision(
                    audit_client.clone(), topic_id.clone(),
                    crate::payments::audit::CurbRecord {
                        v: 1, agent: agent_did.clone(), ts: now_ms,
                        policy: Some("curb.approval-tier".to_string()),
                        method: Some("x402.pay".to_string()),
                        amount: Some(amount_hbar),
                        currency: Some("HBAR".to_string()),
                        counterparty: Some(pay_to.clone()),
                        allowed: Some(true),
                        reason: Some("auto_approved".to_string()),
                        request_id: None,
                    },
                );

                // Step 2: Pay via X402PaymentVault
                let vault: Arc<crate::payments::x402_vault::X402PaymentVault> =
                    match caller.data().x402_vault.clone() {
                    Some(v) => v,
                    None => {
                        let _ = db.release_spend_hold(&agent_did, &pkey);
                        eprintln!("[host_x402_pay] x402_pay capability not enabled");
                        return write_wasm_error(
                            &mut caller,
                            "x402.pay: payment vault not configured — set HEDERA_ACCOUNT_ID and HEDERA_PRIVATE_KEY in the daemon environment"
                        ).await;
                    }
                };

                let (skill_called, task_id) = {
                    let st = caller.data();
                    (st.skill_name.clone(), st.task_id.clone())
                };

                // No separate confirm/deny step exists on this path, so the hold must
                // be committed or released right here — immediately after the pay
                // attempt — rather than deferred to a later resume (the fix already
                // applied to hedera_pay's premature-release bug).
                let payment_result = match vault.pay(requirements.clone(), &skill_called, task_id.as_deref(), None).await {
                    Ok(r) => {
                        let _ = db.commit_spend_hold(&agent_did, &pkey);
                        r
                    }
                    Err(e) => {
                        let _ = db.release_spend_hold(&agent_did, &pkey);
                        eprintln!("[host_x402_pay] payment failed: {}", e);
                        return write_wasm_error(
                            &mut caller,
                            &format!("x402.pay: Hedera payment failed: {}", e)
                        ).await;
                    }
                };

                // Step 3: Retry with PAYMENT-SIGNATURE header
                let retry_resp = match caller.data().http_client
                    .get(&url)
                    .header("PAYMENT-SIGNATURE", &payment_result.payment_token)
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[host_x402_pay] retry GET failed: {}", e);
                        if let Some(db) = caller.data().db.clone()
                            && let Err(db_err) = db.update_payment_status(&payment_result.transaction_id, "delivery_failed") {
                                 eprintln!("[host_x402_pay] failed to update payment status to delivery_failed: {}", db_err);
                            }
                        return write_wasm_error(
                            &mut caller,
                            &format!(
                                "x402.pay: payment sent (tx: {}) but content delivery failed: {}",
                                payment_result.transaction_id, e
                            )
                        ).await;
                    }
                };

                if !retry_resp.status().is_success() {
                    let retry_status = retry_resp.status();
                    eprintln!("[host_x402_pay] retry returned non-200: {}", retry_status);
                    // Read the body so we can surface the server's rejection reason to the user.
                    let retry_body = retry_resp.text().await.unwrap_or_default();
                    eprintln!("[host_x402_pay] retry non-200 body: {}", retry_body);
                    let server_reason = serde_json::from_str::<serde_json::Value>(&retry_body)
                        .ok()
                        .and_then(|v| {
                            v.get("reason")
                                .or_else(|| v.get("error"))
                                .and_then(|r| r.as_str())
                                .map(|s| s.to_string())
                        })
                        .unwrap_or_else(|| retry_body.clone());
                    if let Some(db) = caller.data().db.clone()
                        && let Err(db_err) = db.update_payment_status(&payment_result.transaction_id, "delivery_failed") {
                            eprintln!("[host_x402_pay] failed to update payment status to delivery_failed: {}", db_err);
                        }
                    return write_wasm_error(
                        &mut caller,
                        &format!(
                            "x402.pay: payment sent (tx: {}) but server returned HTTP {} after payment — {} — content not delivered",
                            payment_result.transaction_id, retry_status, server_reason
                        )
                    ).await;
                }

                // READING THE SETTLEMENT CONFIRMATION — the resource server settles
                // the payment (not us) and reports the result via PAYMENT-RESPONSE.
                // This is what actually confirms the payment we logged as PENDING.
                let mut confirmed_tx_id = payment_result.transaction_id.clone();
                let mut confirmed_hashscan_url = payment_result.hashscan_url.clone();
                if let Some(resp_hdr_val) = retry_resp
                    .headers()
                    .get("PAYMENT-RESPONSE")
                    .and_then(|v| v.to_str().ok())
                {
                    if let Ok(decoded) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, resp_hdr_val) {
                        match serde_json::from_slice::<crate::payments::x402_types::SettleResponse>(&decoded) {
                            Ok(settle_resp) if settle_resp.success => {
                                eprintln!("[host_x402_pay] Confirmed settlement via PAYMENT-RESPONSE: {:?}", settle_resp);
                                if let Some(db) = caller.data().db.clone() {
                                    // The server-settled tx id is authoritative; if it differs
                                    // from our locally-generated one, move the payment record
                                    // over to it so status tracking stays keyed correctly.
                                    if settle_resp.transaction_id != payment_result.transaction_id
                                        && let Err(e) = db.update_payment_status(&payment_result.transaction_id, "delivery_failed") {
                                            eprintln!("[host_x402_pay] failed to clear stale pending record: {}", e);
                                    }
                                    if let Err(e) = db.update_payment_status(&settle_resp.transaction_id, "SUCCESS") {
                                        eprintln!("[host_x402_pay] failed to mark payment SUCCESS: {}", e);
                                    }
                                }
                                confirmed_tx_id = settle_resp.transaction_id.clone();
                                confirmed_hashscan_url = format!(
                                    "https://hashscan.io/testnet/transaction/{}",
                                    settle_resp.transaction_id
                                );
                            }
                            Ok(settle_resp) => {
                                eprintln!("[host_x402_pay] Server reported settlement failure: {:?}", settle_resp);
                                if let Some(db) = caller.data().db.clone()
                                    && let Err(e) = db.update_payment_status(&payment_result.transaction_id, "FAILED") {
                                        eprintln!("[host_x402_pay] failed to mark payment FAILED: {}", e);
                                }
                            }
                            Err(e) => {
                                eprintln!("[host_x402_pay] failed to parse PAYMENT-RESPONSE header JSON: {}", e);
                            }
                        }
                    } else {
                        eprintln!("[host_x402_pay] failed to base64-decode PAYMENT-RESPONSE header");
                    }
                } else {
                    // No PAYMENT-RESPONSE header at all — we can't confirm settlement
                    // actually happened even though the server returned 200. Leave the
                    // payment as PENDING rather than silently assuming success.
                    eprintln!("[host_x402_pay] warning: 200 response had no PAYMENT-RESPONSE header — payment left PENDING");
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
                        if let Some(db) = caller.data().db.clone()
                            && let Err(db_err) = db.update_payment_status(&payment_result.transaction_id, "delivery_failed") {
                                eprintln!("[host_x402_pay] failed to update payment status to delivery_failed: {}", db_err);
                            }
                        return write_wasm_error(
                            &mut caller,
                            &format!(
                                "x402.pay: payment sent (tx: {}) but failed to read response body: {}",
                                payment_result.transaction_id, e
                            )
                        ).await;
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
                    "transaction_id": confirmed_tx_id,
                    "hashscan_url": confirmed_hashscan_url,
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
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
        )| {
            Box::new(async move {
                let recipient = match read_wasm_str(&mut caller, recipient_ptr, recipient_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_hedera_pay] bad recipient: {}", e);
                        return 0;
                    }
                };
                let amount_str = match read_wasm_str(&mut caller, amount_ptr, amount_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_hedera_pay] bad amount: {}", e);
                        return 0;
                    }
                };
                let memo = match read_wasm_str(&mut caller, memo_ptr, memo_len) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[host_hedera_pay] bad memo: {}", e);
                        return 0;
                    }
                };
                let amount: f64 = match amount_str.parse() {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("[host_hedera_pay] bad amount format: {}", e);
                        return 0;
                    }
                };

                let vault = match caller.data().payment_vault.clone() {
                    Some(v) => v,
                    None => {
                        eprintln!("[host_hedera_pay] hedera_pay capability not enabled");
                        return 0;
                    }
                };

                let receipt = match vault.pay(&recipient, amount, &memo).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[host_hedera_pay] payment failed: {}", e);
                        return 0;
                    }
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
                }))
                .unwrap_or_default();
                write_wasm_bytes(&mut caller, &bytes).await.unwrap_or(0)
            })
        },
    )?;
    Ok(())
}
