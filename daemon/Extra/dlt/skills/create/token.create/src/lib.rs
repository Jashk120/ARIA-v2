use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_hts_create(
        name_ptr: *const u8,
        name_len: usize,
        symbol_ptr: *const u8,
        symbol_len: usize,
        type_ptr: *const u8,
        type_len: usize,
        decimals_ptr: *const u8,
        decimals_len: usize,
        supply_ptr: *const u8,
        supply_len: usize,
        treasury_ptr: *const u8,
        treasury_len: usize,
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
    name: Option<String>,
    symbol: Option<String>,
    #[serde(default)]
    token_type: String,
    decimals: Option<i64>,
    amount: Option<f64>,
    treasury: Option<String>,
    #[serde(default)]
    memo: String,
}

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    let name = match args.name {
        Some(ref n) if !n.trim().is_empty() => n.clone(),
        _ => {
            return Err("token.create requires a 'name' argument — no default name \
                exists. Provide the token name (e.g. Green Bond Token)."
                .to_string());
        }
    };

    let symbol = match args.symbol {
        Some(ref s) if !s.trim().is_empty() => s.clone(),
        _ => {
            return Err("token.create requires a 'symbol' argument — no default symbol \
                exists. Provide the token symbol (e.g. GBOND)."
                .to_string());
        }
    };

    let amount = match args.amount {
        Some(a) if a >= 0.0 => a,
        Some(_) => {
            return Err("token.create requires 'amount' to be a non-negative number.".to_string());
        }
        None => {
            return Err("token.create requires an 'amount' argument — no default supply \
                exists. Provide the initial supply (fungible) or max supply (nft)."
                .to_string());
        }
    };

    let decimals = args.decimals.unwrap_or(0);
    if decimals < 0 {
        return Err("token.create requires 'decimals' to be a non-negative integer.".to_string());
    }

    let created = hts_create(
        &name,
        &symbol,
        args.token_type.trim(),
        &decimals.to_string(),
        &amount.to_string(),
        args.treasury.as_deref().unwrap_or("").trim(),
        &args.memo,
    )?;
    serde_json::from_str(&created).map_err(|e| format!("Bad token.create response JSON: {}", e))
}

fn hts_create(
    name: &str,
    symbol: &str,
    token_type: &str,
    decimals: &str,
    supply: &str,
    treasury: &str,
    memo: &str,
) -> Result<String, String> {
    let packed = unsafe {
        let n = name.as_bytes();
        let s = symbol.as_bytes();
        let t = token_type.as_bytes();
        let d = decimals.as_bytes();
        let sp = supply.as_bytes();
        let tr = treasury.as_bytes();
        let m = memo.as_bytes();
        host_hts_create(
            n.as_ptr(),
            n.len(),
            s.as_ptr(),
            s.len(),
            t.as_ptr(),
            t.len(),
            d.as_ptr(),
            d.len(),
            sp.as_ptr(),
            sp.len(),
            tr.as_ptr(),
            tr.len(),
            m.as_ptr(),
            m.len(),
        )
    };
    read_packed(packed)
}

fn read_packed(packed: u64) -> Result<String, String> {
    let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize);
    if ptr == 0 {
        return Err("host call failed — hts capability may not be enabled \
            (missing HEDERA_ACCOUNT_ID / HEDERA_PRIVATE_KEY) or the token create failed"
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
