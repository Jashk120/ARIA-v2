//! run.exec — sandboxed code-execution skill.
//! Runs Python or shell snippets via the host `host_exec_run` function and
//! returns captured stdout/stderr plus exit status. Compiled to WASM; all
//! process execution happens on the host inside the FsSandbox working dir.

use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    /// host_exec_run(lang_ptr, lang_len, code_ptr, code_len, timeout_ms)
    ///   -> packed(ptr, len) of JSON bytes {stdout,stderr,exit_code,timed_out}
    ///      or {"error": ...}, or 0 on transport error
    fn host_exec_run(
        lang_ptr: *const u8,
        lang_len: usize,
        code_ptr: *const u8,
        code_len: usize,
        timeout_ms: i32,
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
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    };

    let len = output.len();
    let ptr = to_wasm_ptr(output);
    ((ptr as u64) << 32) | (len as u64)
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Input {
    /// "python" or "sh".
    language: String,
    /// Source code to execute.
    code: String,
    /// Max wall-clock time in milliseconds (default 10000, clamped 100..60000).
    #[serde(default = "default_timeout")]
    timeout_ms: i64,
}

fn default_timeout() -> i64 {
    10000
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    if args.code.is_empty() {
        return Err("No code provided — pass 'code' with source to execute".to_string());
    }

    let timeout_ms = args.timeout_ms.clamp(100, 60000) as i32;

    let raw_json = exec(&args.language, &args.code, timeout_ms)?;

    let out: Value =
        serde_json::from_str(&raw_json).map_err(|e| format!("Bad JSON from host_exec_run: {}", e))?;

    if let Some(err) = out.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }

    Ok(out)
}

// ── Host call wrapper ─────────────────────────────────────────────────────────

fn exec(language: &str, code: &str, timeout_ms: i32) -> Result<String, String> {
    let packed = unsafe {
        let l = language.as_bytes();
        let c = code.as_bytes();
        host_exec_run(l.as_ptr(), l.len(), c.as_ptr(), c.len(), timeout_ms)
    };

    if packed == 0 {
        return Err(
            "host_exec_run returned NULL — execution failed (denied, unsupported language, or host error)"
                .to_string(),
        );
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
