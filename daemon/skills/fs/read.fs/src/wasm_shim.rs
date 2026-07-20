//! wasm_shim — host FFI glue. Only compiled for wasm32 targets; everything
//! else (`extract`, format parsers) lives in lib.rs and is target-agnostic.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{extract, ReadOptions};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    /// host_fs_read(path_ptr, path_len) -> packed(ptr, len) of file bytes, or 0 on error
    fn host_fs_read(path_ptr: *const u8, path_len: usize) -> u64;
}

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

#[derive(Deserialize)]
struct Input {
    path: String,
    #[serde(default)]
    line_range: Option<(usize, usize)>,
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
    #[serde(default = "default_max_rows")]
    max_rows_per_sheet: usize,
}

fn default_max_bytes() -> usize {
    1024 * 1024
}
fn default_max_rows() -> usize {
    500
}

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    let raw = fs_read(&args.path)?;

    let opts = ReadOptions {
        line_range: args.line_range,
        max_bytes: args.max_bytes,
        max_rows_per_sheet: args.max_rows_per_sheet,
    };

    let result = extract(&args.path, &raw, &opts)?;
    serde_json::to_value(result).map_err(|e| format!("Failed to serialize result: {}", e))
}

fn fs_read(path: &str) -> Result<Vec<u8>, String> {
    let packed = unsafe {
        let p = path.as_bytes();
        host_fs_read(p.as_ptr(), p.len())
    };

    if packed == 0 {
        return Err(format!(
            "host_fs_read returned NULL — read of \"{}\" failed (access denied, not found, or not a regular file)",
            path
        ));
    }

    let (ptr, len) = unpack_ptr_len(packed);
    let bytes = unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len);
        let owned = slice.to_vec();
        let _ = Vec::from_raw_parts(ptr as *mut u8, len, len);
        owned
    };

    Ok(bytes)
}

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