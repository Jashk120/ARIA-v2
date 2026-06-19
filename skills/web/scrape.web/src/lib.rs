//! scrape.web — fetch and extract readable text from a URL
//! No external deps — HTML stripping done manually

use serde::Deserialize;
use serde_json::{Value, json};

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_http_get(
        url_ptr: *const u8,
        url_len: usize,
        headers_ptr: *const u8,
        headers_len: usize,
    ) -> u64;

    fn host_free(ptr: *mut u8);
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
    url: String,
    /// Max chars of extracted text to return (default 4000, max 10000)
    #[serde(default = "default_max_chars")]
    max_chars: usize,
}

fn default_max_chars() -> usize {
    4000
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    let max_chars = args.max_chars.min(10000);

    let headers = json!({
        "User-Agent": "Mozilla/5.0 (compatible; ARIA-agent/0.5)",
        "Accept": "text/html,application/xhtml+xml",
    })
    .to_string();

    let html = http_get(&args.url, &headers)?;
    let text = extract_text(&html, max_chars);

    if text.is_empty() {
        return Err("No readable text found on page".to_string());
    }

    Ok(json!({
        "url":   args.url,
        "text":  text,
        "chars": text.len(),
    }))
}

// ── Text extraction ───────────────────────────────────────────────────────────

fn extract_text(html: &str, max_chars: usize) -> String {
    // 1. Remove script/style blocks entirely
    let html = remove_blocks(html, "script");
    let html = remove_blocks(&html, "style");
    let html = remove_blocks(&html, "nav");
    let html = remove_blocks(&html, "footer");
    let html = remove_blocks(&html, "header");

    // 2. Strip remaining tags
    let mut out = String::new();
    let mut in_tag = false;
    let mut last_was_space = false;

    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => {
                in_tag = false;
            }
            _ if in_tag => {}
            '\n' | '\r' | '\t' => {
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
            }
            ' ' => {
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
            }
            c => {
                out.push(c);
                last_was_space = false;
            }
        }
    }

    // 3. Decode entities
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ");

    // 4. Trim and truncate
    let out = out.trim().to_string();
    if out.len() > max_chars {
        let mut end = max_chars.min(out.len());
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = &out[..end];
        let last_space = truncated.rfind(' ').unwrap_or(end);
        format!("{}…", &truncated[..last_space])
    } else {
        out
    }
}

fn remove_blocks(html: &str, tag: &str) -> String {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut out = String::new();
    let mut pos = 0;
    let lower = html.to_lowercase();

    while pos < html.len() {
        if let Some(start) = lower[pos..].find(&open).map(|i| i + pos) {
            out.push_str(&html[pos..start]);
            if let Some(end) = lower[start..].find(&close).map(|i| i + start + close.len()) {
                pos = end;
            } else {
                break;
            }
        } else {
            out.push_str(&html[pos..]);
            break;
        }
    }
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn http_get(url: &str, headers_json: &str) -> Result<String, String> {
    let packed = unsafe {
        let u = url.as_bytes();
        let h = headers_json.as_bytes();
        host_http_get(u.as_ptr(), u.len(), h.as_ptr(), h.len())
    };

    if packed == 0 {
        return Err("HTTP request failed".to_string());
    }

    let (ptr, len) = unpack_ptr_len(packed);
    let s = unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len);
        let owned = String::from_utf8_lossy(slice).to_string();
        // Since we now use a Guest-Allocated buffer, we should free it locally.
        let _ = Vec::from_raw_parts(ptr as *mut u8, len, len);
        owned
    };

    Ok(s)
}

fn unpack_ptr_len(packed: u64) -> (usize, usize) {
    ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize)
}

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}
