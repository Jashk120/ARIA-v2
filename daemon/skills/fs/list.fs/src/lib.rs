//! list.fs — directory listing skill
//! Lists entries of a directory. Compiled to WASM.
//! All filesystem I/O goes through host_fs_list.

use serde::{
    Deserialize,
    Serialize,
};
use serde_json::{
    Value,
    json,
};

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    /// host_fs_list(path_ptr, path_len) -> packed(ptr, len) of JSON array
    /// bytes, or 0 on error. Each entry: {"name":"...","is_dir":bool,"size":number}
    fn host_fs_list(path_ptr: *const u8, path_len: usize) -> u64;
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
    /// Directory path to list (relative to fs_root or absolute).
    path: String,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    let raw_json = fs_list(&args.path)?;

    let entries: Vec<Entry> = serde_json::from_str(&raw_json)
        .map_err(|e| format!("Bad JSON from host_fs_list: {}", e))?;

    Ok(json!({
        "path": args.path,
        "entries": entries,
    }))
}

// ── Host call wrapper ─────────────────────────────────────────────────────────

fn fs_list(path: &str) -> Result<String, String> {
    let packed = unsafe {
        let p = path.as_bytes();
        host_fs_list(p.as_ptr(), p.len())
    };

    if packed == 0 {
        return Err(format!(
            "host_fs_list returned NULL — list of \"{}\" failed (access denied or path not found)",
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
