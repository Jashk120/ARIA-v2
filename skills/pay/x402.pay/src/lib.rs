use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_x402_pay(url_ptr: *const u8, url_len: usize) -> u64;
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
    url: Option<String>,
}

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    // url is required — there is no default target. Failing here (before touching
    // the network) prevents silently spending funds against an unintended server.
    let target_url = match args.url {
        Some(ref u) if !u.trim().is_empty() => u.clone(),
        _ => {
            return Err("pay.x402 requires a 'url' argument — no default target exists. \
                Provide the full URL of the resource to fetch or pay for. \
                If you do not have a URL yet, search for one or ask the user \
                before calling this skill again."
                .to_string());
        }
    };

    let paid = x402_pay(&target_url)?;
    serde_json::from_str(&paid).map_err(|e| format!("Bad x402 response JSON: {}", e))
}

fn x402_pay(url: &str) -> Result<String, String> {
    let packed = unsafe {
        let u = url.as_bytes();
        host_x402_pay(u.as_ptr(), u.len())
    };
    read_packed(packed)
}

fn read_packed(packed: u64) -> Result<String, String> {
    let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize);
    if ptr == 0 {
        return Err("host call failed".to_string());
    }
    let s = unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len);
        let owned = String::from_utf8_lossy(slice).to_string();
        let _ = Vec::from_raw_parts(ptr as *mut u8, len, len);
        owned
    };
    Ok(s)
}

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}
