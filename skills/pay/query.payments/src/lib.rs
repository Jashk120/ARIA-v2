use serde::Deserialize;
use serde_json::{Value, json};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_db_query(type_ptr: *const u8, type_len: usize, params_ptr: *const u8, params_len: usize) -> u64;
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
        std::str::from_utf8(std::slice::from_raw_parts(input_ptr, input_len)).unwrap_or("")
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
    #[serde(default = "default_days")]
    days: i64,
}
fn default_days() -> i64 { 7 }

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).unwrap_or(Input { days: 7 });
    let params = json!({ "days": args.days }).to_string();

    let packed = unsafe {
        let t = b"payments_recent";
        let p = params.as_bytes();
        host_db_query(t.as_ptr(), t.len(), p.as_ptr(), p.len())
    };
    let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize);
    if ptr == 0 {
        return Err("db query failed".to_string());
    }
    let s = unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len);
        let owned = String::from_utf8_lossy(slice).to_string();
        let _ = Vec::from_raw_parts(ptr as *mut u8, len, len);
        owned
    };
    serde_json::from_str(&s).map_err(|e| format!("bad db response: {}", e))
}

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}