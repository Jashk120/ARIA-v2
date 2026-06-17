//! find.fs — file search skill
//! Searches a directory tree by filename pattern or file content.
//! Compiled to WASM. All filesystem I/O goes through host_fs_find.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    /// host_fs_find(path_ptr, path_len, query_ptr, query_len, mode_ptr, mode_len)
    ///   -> packed(ptr, len) of JSON array bytes, or 0 on error
    fn host_fs_find(
        path_ptr: *const u8, path_len: usize,
        query_ptr: *const u8, query_len: usize,
        mode_ptr: *const u8, mode_len: usize,
    ) -> u64;
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn alloc(size: usize) -> *mut u8 {
    let mut buf = Vec::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[unsafe(no_mangle)]
pub extern "C" fn run(input_ptr: *const u8, input_len: usize) -> u64 {
    let input = unsafe {
        let slice = std::slice::from_raw_parts(input_ptr, input_len);
        std::str::from_utf8(slice).unwrap_or("")
    };

    let output = match execute(input) {
        Ok(v)  => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    };

    let len = output.len();
    let ptr = to_wasm_ptr(output);
    ((ptr as u64) << 32) | (len as u64)
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Input {
    /// Directory path to search in (relative to fs_root or absolute).
    path: String,
    /// Search term — matched against filenames (mode=name) or file contents (mode=content).
    query: String,
    /// "name" for filename matching, "content" for grep-style content search.
    #[serde(default = "default_mode")]
    mode: String,
    /// Max results to return.
    #[serde(default = "default_max")]
    max_results: usize,
}

fn default_mode() -> String { "name".to_string() }
fn default_max() -> usize { 20 }

#[derive(Serialize)]
struct Match {
    path:    String,
    preview: String,
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input)
        .map_err(|e| format!("Invalid input: {}", e))?;

    let mode = match args.mode.as_str() {
        "name" | "content" => args.mode.as_str(),
        _ => "name",
    };

    let raw_json = fs_find(&args.path, &args.query, mode)?;

    let all: Vec<Value> = serde_json::from_str(&raw_json)
        .map_err(|e| format!("Bad JSON from host_fs_find: {}", e))?;

    let max = args.max_results.min(100);
    let truncated = all.len() > max;

    let matches: Vec<Match> = all.into_iter()
        .take(max)
        .map(|entry| Match {
            path:    entry["path"].as_str().unwrap_or("").to_string(),
            preview: entry["preview"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    if matches.is_empty() {
        return Err(format!(
            "No files found matching \"{}\" (mode={}) in {}",
            args.query, mode, args.path
        ));
    }

    Ok(json!({
        "matches": matches,
        "truncated": truncated,
    }))
}

// ── Host call wrapper ─────────────────────────────────────────────────────────

fn fs_find(path: &str, query: &str, mode: &str) -> Result<String, String> {
    let packed = unsafe {
        let p = path.as_bytes();
        let q = query.as_bytes();
        let m = mode.as_bytes();
        host_fs_find(
            p.as_ptr(), p.len(),
            q.as_ptr(), q.len(),
            m.as_ptr(), m.len(),
        )
    };

    if packed == 0 {
        return Err(format!(
            "host_fs_find returned NULL — search in \"{}\" failed (access denied or path not found)",
            path
        ));
    }

    let (ptr, len) = unpack_ptr_len(packed);
    let s = unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len);
        let owned = String::from_utf8_lossy(slice).to_string();
        // Free the guest-allocated buffer
        let _ = Vec::from_raw_parts(ptr as *mut u8, len, len);
        owned
    };

    Ok(s)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unpack_ptr_len(packed: u64) -> (usize, usize) {
    ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize)
}

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}
