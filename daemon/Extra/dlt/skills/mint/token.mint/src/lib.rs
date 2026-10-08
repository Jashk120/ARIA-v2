use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_hts_mint(
        token_ptr: *const u8,
        token_len: usize,
        amount_ptr: *const u8,
        amount_len: usize,
        metadata_ptr: *const u8,
        metadata_len: usize,
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
    token_id: Option<String>,
    recipient: Option<String>,
    amount: Option<f64>,
    #[serde(default)]
    metadata: String,
}

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    let token_id = match args.token_id {
        Some(ref t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            return Err("token.mint requires a 'token_id' argument — no default token \
                exists. Provide the HTS token id to mint (e.g. 0.0.1234)."
                .to_string());
        }
    };

    let recipient = match args.recipient {
        Some(ref r) if !r.trim().is_empty() => r.trim().to_string(),
        _ => {
            return Err("token.mint requires a 'recipient' argument — no default account \
                exists. Provide the Hedera AccountId the minted supply is intended for."
                .to_string());
        }
    };

    let is_nft = !args.metadata.trim().is_empty();
    if !is_nft {
        match args.amount {
            Some(a) if a > 0.0 => {}
            Some(_) => {
                return Err("token.mint requires 'amount' to be a positive number.".to_string());
            }
            None => {
                return Err("token.mint requires an 'amount' argument for fungible mints — \
                    no default amount exists. Provide the human token units to mint."
                    .to_string());
            }
        }
    }

    let amount_str = args.amount.map(|a| a.to_string()).unwrap_or_default();
    let minted = hts_mint(&token_id, &amount_str, args.metadata.trim())?;
    let mut result: Value =
        serde_json::from_str(&minted).map_err(|e| format!("Bad token.mint response JSON: {}", e))?;
    // The mint lands in the treasury on-chain; echo the intended recipient so
    // the agent's confirmation context stays attached to the result.
    if let Some(obj) = result.as_object_mut() {
        obj.insert("recipient".to_string(), json!(recipient));
    }
    Ok(result)
}

fn hts_mint(token_id: &str, amount: &str, metadata: &str) -> Result<String, String> {
    let packed = unsafe {
        let t = token_id.as_bytes();
        let a = amount.as_bytes();
        let m = metadata.as_bytes();
        host_hts_mint(t.as_ptr(), t.len(), a.as_ptr(), a.len(), m.as_ptr(), m.len())
    };
    read_packed(packed)
}

fn read_packed(packed: u64) -> Result<String, String> {
    let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize);
    if ptr == 0 {
        return Err("host call failed — hts capability may not be enabled \
            (missing HEDERA_ACCOUNT_ID / HEDERA_PRIVATE_KEY) or the mint failed"
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
