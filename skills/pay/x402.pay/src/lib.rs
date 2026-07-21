use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_http_get(url_ptr: *const u8, url_len: usize, headers_ptr: *const u8, headers_len: usize) -> u64;
    fn host_hedera_pay(recipient_ptr: *const u8, recipient_len: usize, amount_ptr: *const u8, amount_len: usize, memo_ptr: *const u8, memo_len: usize) -> u64;
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
    url: String,
}

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    // 1. First attempt — expect HTTP 402 with payment details in the body
    let first = http_get(&args.url, "{}")?;
    let payment_req: Value = serde_json::from_str(&first)
        .map_err(|e| format!("Expected 402 payment-required JSON: {}", e))?;

    let recipient = payment_req["recipient"].as_str().ok_or("missing recipient in 402 body")?;
    let amount = payment_req["amount_hbar"].as_f64().ok_or("missing amount_hbar in 402 body")?;
    let resource_id = payment_req["resource_id"].as_str().unwrap_or("");

    // 2. Pay via host function
    let receipt_json = hedera_pay(recipient, amount, resource_id)?;
    let receipt: Value = serde_json::from_str(&receipt_json).map_err(|e| format!("Bad receipt JSON: {}", e))?;
    let tx_id = receipt["transaction_id"].as_str().unwrap_or("").to_string();

    // 3. Retry with proof of payment
    let headers = json!({ "X-PAYMENT-PROOF": tx_id }).to_string();
    let unlocked = http_get(&args.url, &headers)?;
    let data: Value = serde_json::from_str(&unlocked).unwrap_or(json!({ "raw": unlocked }));

    Ok(json!({
        "data": data,
        "transaction_id": tx_id,
        "hashscan_url": receipt["hashscan_url"],
    }))
}

fn http_get(url: &str, headers_json: &str) -> Result<String, String> {
    let packed = unsafe {
        let u = url.as_bytes();
        let h = headers_json.as_bytes();
        host_http_get(u.as_ptr(), u.len(), h.as_ptr(), h.len())
    };
    read_packed(packed)
}

fn hedera_pay(recipient: &str, amount: f64, memo: &str) -> Result<String, String> {
    let amount_str = amount.to_string();
    let packed = unsafe {
        let r = recipient.as_bytes();
        let a = amount_str.as_bytes();
        let m = memo.as_bytes();
        host_hedera_pay(r.as_ptr(), r.len(), a.as_ptr(), a.len(), m.as_ptr(), m.len())
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