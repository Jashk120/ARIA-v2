use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_chain_query(
        kind_ptr: *const u8,
        kind_len: usize,
        id_ptr: *const u8,
        id_len: usize,
        opts_ptr: *const u8,
        opts_len: usize,
    ) -> u64;
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
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    order: Option<String>,
    #[serde(default, rename = "type")]
    tx_type: Option<String>,
    #[serde(default)]
    token_id: Option<String>,
    #[serde(default)]
    topic_id: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

fn execute(input: &str) -> Result<Value, String> {
    let args: Input =
        serde_json::from_str(input).map_err(|e| format!("bad input: {}", e))?;
    if args.kind.trim().is_empty() {
        return Err("kind is required".to_string());
    }
    let mut opts = serde_json::Map::new();
    if let Some(limit) = args.limit {
        opts.insert("limit".to_string(), json!(limit));
    }
    if let Some(order) = args.order.filter(|s| !s.trim().is_empty()) {
        opts.insert("order".to_string(), json!(order));
    }
    if let Some(t) = args.tx_type.filter(|s| !s.trim().is_empty()) {
        opts.insert("type".to_string(), json!(t));
    }
    if let Some(t) = args.token_id.filter(|s| !s.trim().is_empty()) {
        opts.insert("token_id".to_string(), json!(t));
    }
    if let Some(t) = args.topic_id.filter(|s| !s.trim().is_empty()) {
        opts.insert("topic_id".to_string(), json!(t));
    }
    if let Some(a) = args.account_id.filter(|s| !s.trim().is_empty()) {
        opts.insert("account_id".to_string(), json!(a));
    }
    let opts_str = Value::Object(opts).to_string();

    let packed = unsafe {
        let k = args.kind.as_bytes();
        let i = args.id.as_bytes();
        let o = opts_str.as_bytes();
        host_chain_query(k.as_ptr(), k.len(), i.as_ptr(), i.len(), o.as_ptr(), o.len())
    };
    let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize);
    if ptr == 0 {
        return Err("host chain query failed".to_string());
    }
    let s = unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len);
        let owned = String::from_utf8_lossy(slice).to_string();
        let _ = Vec::from_raw_parts(ptr as *mut u8, len, len);
        owned
    };
    serde_json::from_str(&s).map_err(|e| format!("bad chain response: {}", e))
}

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}
