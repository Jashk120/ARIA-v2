use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_hedera_pay(
        recipient_ptr: *const u8,
        recipient_len: usize,
        amount_ptr: *const u8,
        amount_len: usize,
        memo_ptr: *const u8,
        memo_len: usize,
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
    recipient: Option<String>,
    amount: Option<f64>,
    #[serde(default)]
    memo: String,
}

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    let recipient = match args.recipient {
        Some(ref r) if !r.trim().is_empty() => r.clone(),
        _ => {
            return Err("transfer.pay requires a 'recipient' argument — no default account \
                exists. Provide the Hedera AccountId to pay (e.g. 0.0.1234)."
                .to_string());
        }
    };

    let amount = match args.amount {
        Some(a) if a > 0.0 => a,
        Some(_) => {
            return Err("transfer.pay requires 'amount' to be a positive number.".to_string());
        }
        None => {
            return Err("transfer.pay requires an 'amount' argument — no default amount \
                exists. Provide the HBAR amount to send (e.g. 1.5)."
                .to_string());
        }
    };

    let paid = hedera_pay(&recipient, amount, &args.memo)?;
    serde_json::from_str(&paid).map_err(|e| format!("Bad transfer response JSON: {}", e))
}

fn hedera_pay(recipient: &str, amount: f64, memo: &str) -> Result<String, String> {
    let packed = unsafe {
        let r = recipient.as_bytes();
        let a = amount.to_string();
        let a = a.as_bytes();
        let m = memo.as_bytes();
        host_hedera_pay(r.as_ptr(), r.len(), a.as_ptr(), a.len(), m.as_ptr(), m.len())
    };
    read_packed(packed)
}

fn read_packed(packed: u64) -> Result<String, String> {
    let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize);
    if ptr == 0 {
        return Err("host call failed — hedera_pay capability may not be enabled \
            (missing HEDERA_ACCOUNT_ID / HEDERA_PRIVATE_KEY) or the transfer failed"
            .to_string());
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
