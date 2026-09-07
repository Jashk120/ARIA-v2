//! write.fs — file write skill
//! Writes (or appends) content to a file. Compiled to WASM.
//! All filesystem I/O goes through host_fs_write.

use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    /// host_fs_write(path_ptr, path_len, content_ptr, content_len, mode_ptr, mode_len)
    ///   -> 1 on success, 0 on error
    fn host_fs_write(
        path_ptr: *const u8,
        path_len: usize,
        content_ptr: *const u8,
        content_len: usize,
        mode_ptr: *const u8,
        mode_len: usize,
    ) -> i32;
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
    /// Destination file path (relative to fs_root or absolute).
    path: String,
    /// Content to write.
    content: String,
    /// "overwrite" (default) or "append".
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_mode() -> String {
    "overwrite".to_string()
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    let mode = match args.mode.as_str() {
        "overwrite" | "append" => args.mode.as_str(),
        _ => "overwrite",
    };

    let bytes_written = args.content.len();

    fs_write(&args.path, &args.content, mode)?;

    Ok(json!({
        "path": args.path,
        "bytes_written": bytes_written,
        "mode": mode,
    }))
}

// ── Host call wrapper ─────────────────────────────────────────────────────────

fn fs_write(path: &str, content: &str, mode: &str) -> Result<(), String> {
    let rc = unsafe {
        let p = path.as_bytes();
        let c = content.as_bytes();
        let m = mode.as_bytes();
        host_fs_write(p.as_ptr(), p.len(), c.as_ptr(), c.len(), m.as_ptr(), m.len())
    };

    if rc != 1 {
        return Err(format!(
            "host_fs_write returned {} — write to \"{}\" failed (access denied or invalid path)",
            rc, path
        ));
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}
