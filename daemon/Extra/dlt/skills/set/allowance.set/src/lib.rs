use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_hts_approve(
        asset_ptr: *const u8,
        asset_len: usize,
        spender_ptr: *const u8,
        spender_len: usize,
        amount_ptr: *const u8,
        amount_len: usize,
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
    asset: Option<String>,
    spender: Option<String>,
    amount: Option<f64>,
}

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    let asset = match args.asset {
        Some(ref a) if !a.trim().is_empty() => a.trim().to_string(),
        _ => {
            return Err("allowance.set requires an 'asset' argument — no default asset \
                exists. Provide hbar or an HTS token id (e.g. 0.0.1234)."
                .to_string());
        }
    };

    let spender = match args.spender {
        Some(ref s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            return Err("allowance.set requires a 'spender' argument — no default spender \
                exists. Provide the Hedera AccountId allowed to spend (e.g. 0.0.1234)."
                .to_string());
        }
    };

    let amount = match args.amount {
        Some(a) if a > 0.0 => a,
        Some(_) => {
            return Err("allowance.set requires 'amount' to be a positive number.".to_string());
        }
        None => {
            return Err("allowance.set requires an 'amount' argument — no default amount \
                exists. Provide the HBAR or human token units to approve."
                .to_string());
        }
    };

    let approved = hts_approve(&asset, &spender, &amount.to_string())?;
    serde_json::from_str(&approved)
        .map_err(|e| format!("Bad allowance.set response JSON: {}", e))
}

fn hts_approve(asset: &str, spender: &str, amount: &str) -> Result<String, String> {
    let packed = unsafe {
        let a = asset.as_bytes();
        let s = spender.as_bytes();
        let m = amount.as_bytes();
        host_hts_approve(a.as_ptr(), a.len(), s.as_ptr(), s.len(), m.as_ptr(), m.len())
    };
    read_packed(packed)
}

fn read_packed(packed: u64) -> Result<String, String> {
    let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize);
    if ptr == 0 {
        return Err("host call failed — hts capability may not be enabled \
            (missing HEDERA_ACCOUNT_ID / HEDERA_PRIVATE_KEY) or the approval failed"
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
