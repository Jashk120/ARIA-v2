//! search.web — web search skill
//! Default: SearXNG (self-hosted or public instance, no key needed)
//! Optional: Brave Search API (set brave_api_key in ARIA config for better results)
//!
//! Compiled to WASM. All HTTP goes through host_http_get.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    fn host_http_get(
        url_ptr: *const u8, url_len: usize,
        headers_ptr: *const u8, headers_len: usize,
    ) -> *mut u8;

    fn host_free(ptr: *mut u8);
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn run(input_ptr: *const u8, input_len: usize) -> *mut u8 {
    let input = unsafe {
        let slice = std::slice::from_raw_parts(input_ptr, input_len);
        std::str::from_utf8(slice).unwrap_or("")
    };

    let output = match execute(input) {
        Ok(v)  => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    };

    to_wasm_ptr(output)
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default = "default_max")]
    max_results: u8,
    // Host injects searxng_url from config, falls back to public instance
    #[serde(default = "default_searxng_url")]
    searxng_url: String,
    // Host injects this if brave_api_key is set in db, otherwise empty string
    #[serde(default)]
    brave_api_key: String,
}

fn default_max() -> u8 { 5 }
fn default_searxng_url() -> String { "https://searx.be".to_string() }

#[derive(Serialize)]
struct SearchResult {
    title:   String,
    url:     String,
    snippet: String,
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input)
        .map_err(|e| format!("Invalid input: {}", e))?;

    let max = args.max_results.min(10);

    if !args.brave_api_key.is_empty() {
        search_brave(&args.query, max, &args.brave_api_key)
    } else {
        search_searxng(&args.query, max, &args.searxng_url)
    }
}

// ── SearXNG (default, no key) ─────────────────────────────────────────────────

fn search_searxng(query: &str, max: u8, base_url: &str) -> Result<Value, String> {
    let url = format!(
        "{}/search?q={}&format=json&pageno=1",
        base_url.trim_end_matches('/'),
        url_encode(query)
    );

    let headers = json!({
        "Accept": "application/json",
        "User-Agent": "Mozilla/5.0 (compatible; ARIA-agent/0.5)",
    }).to_string();

    let body = http_get(&url, &headers)?;

    let raw: Value = serde_json::from_str(&body)
        .map_err(|e| format!("Bad JSON from SearXNG: {}", e))?;

    let results: Vec<SearchResult> = raw["results"]
        .as_array()
        .ok_or("No results array in SearXNG response")?
        .iter()
        .take(max as usize)
        .map(|r| SearchResult {
            title:   r["title"].as_str().unwrap_or("").to_string(),
            url:     r["url"].as_str().unwrap_or("").to_string(),
            snippet: r["content"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    if results.is_empty() {
        return Err("No results from SearXNG".to_string());
    }

    Ok(json!({ "results": results, "source": "searxng" }))
}

// ── Brave (optional, better quality) ─────────────────────────────────────────

fn search_brave(query: &str, max: u8, api_key: &str) -> Result<Value, String> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        url_encode(query),
        max
    );

    let headers = json!({
        "Accept": "application/json",
        "X-Subscription-Token": api_key,
    }).to_string();

    let body = http_get(&url, &headers)?;

    let raw: Value = serde_json::from_str(&body)
        .map_err(|e| format!("Bad JSON from Brave: {}", e))?;

    let results: Vec<SearchResult> = raw["web"]["results"]
        .as_array()
        .ok_or("No results in Brave response")?
        .iter()
        .take(max as usize)
        .map(|r| SearchResult {
            title:   r["title"].as_str().unwrap_or("").to_string(),
            url:     r["url"].as_str().unwrap_or("").to_string(),
            snippet: r["description"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    Ok(json!({ "results": results, "source": "brave" }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn http_get(url: &str, headers_json: &str) -> Result<String, String> {
    let ptr = unsafe {
        let u = url.as_bytes();
        let h = headers_json.as_bytes();
        host_http_get(u.as_ptr(), u.len(), h.as_ptr(), h.len())
    };

    if ptr.is_null() {
        return Err("HTTP request failed".to_string());
    }

    let s = unsafe {
        let cstr = std::ffi::CStr::from_ptr(ptr as *const i8);
        let owned = cstr.to_string_lossy().to_string();
        host_free(ptr);
        owned
    };

    Ok(s)
}

fn url_encode(s: &str) -> String {
    s.chars().map(|c| match c {
        'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
        ' ' => "+".to_string(),
        c   => format!("%{:02X}", c as u32),
    }).collect()
}

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}