//! search.web — web search skill
//! Default: DuckDuckGo HTML scraping (no key needed)
//! Optional: Brave Search API (set brave_api_key in ARIA config for better results)
//!
//! Compiled to WASM. All HTTP goes through host_http_get.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
extern "C" {
    fn host_http_get(
        url_ptr: *const u8, url_len: usize,
        headers_ptr: *const u8, headers_len: usize,
    ) -> *mut u8;

    fn host_free(ptr: *mut u8);
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[no_mangle]
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
    // Host injects this if brave_api_key is set in db, otherwise empty string
    #[serde(default)]
    brave_api_key: String,
}

fn default_max() -> u8 { 5 }

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

    if args.brave_api_key.is_empty() {
        search_ddg(&args.query, max)
    } else {
        search_brave(&args.query, max, &args.brave_api_key)
    }
}

// ── DuckDuckGo (default, no key) ──────────────────────────────────────────────

fn search_ddg(query: &str, max: u8) -> Result<Value, String> {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        url_encode(query)
    );

    // DDG requires a real user-agent or it blocks
    let headers = json!({
        "User-Agent": "Mozilla/5.0 (compatible; ARIA-agent/0.5)",
        "Accept": "text/html",
    }).to_string();

    let body = http_get(&url, &headers)?;

    parse_ddg_html(&body, max)
}

fn parse_ddg_html(html: &str, max: u8) -> Result<Value, String> {
    // DDG results look like:
    // <a class="result__a" href="...">Title</a>
    // <a class="result__snippet">Snippet text</a>
    // Simple substring parsing — no regex, no deps

    let mut results: Vec<SearchResult> = Vec::new();
    let mut pos = 0;

    while results.len() < max as usize {
        // Find next result link
        let anchor_marker = "class=\"result__a\"";
        let Some(a_start) = html[pos..].find(anchor_marker).map(|i| i + pos) else { break };

        // Extract href
        let href_start = match html[..a_start].rfind("href=\"") {
            Some(i) => i + 6,
            None => { pos = a_start + 1; continue; }
        };
        let href_end = match html[href_start..].find('"') {
            Some(i) => i + href_start,
            None => { pos = a_start + 1; continue; }
        };
        let url = html[href_start..href_end].to_string();

        // Skip DDG internal links
        if url.starts_with("/") || url.contains("duckduckgo.com") {
            pos = a_start + 1;
            continue;
        }

        // Extract title (text between > and </a>)
        let title_start = match html[a_start..].find('>') {
            Some(i) => i + a_start + 1,
            None => { pos = a_start + 1; continue; }
        };
        let title_end = match html[title_start..].find("</a>") {
            Some(i) => i + title_start,
            None => { pos = a_start + 1; continue; }
        };
        let title = strip_tags(&html[title_start..title_end]);

        // Find snippet after this result
        let snippet_marker = "result__snippet";
        let snippet = if let Some(s_start) = html[title_end..].find(snippet_marker).map(|i| i + title_end) {
            let text_start = match html[s_start..].find('>') {
                Some(i) => i + s_start + 1,
                None => s_start,
            };
            let text_end = match html[text_start..].find("</a>") {
                Some(i) => i + text_start,
                None => (text_start + 200).min(html.len()),
            };
            strip_tags(&html[text_start..text_end])
        } else {
            String::new()
        };

        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult { title, url, snippet });
        }

        pos = title_end + 1;
    }

    if results.is_empty() {
        return Err("No results found — DDG may have changed its HTML structure".to_string());
    }

    Ok(json!({ "results": results, "source": "ddg" }))
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

/// Strip HTML tags from a string
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // Decode common HTML entities
    out.replace("&amp;", "&")
       .replace("&lt;", "<")
       .replace("&gt;", ">")
       .replace("&quot;", "\"")
       .replace("&#x27;", "'")
       .replace("&nbsp;", " ")
       .trim()
       .to_string()
}

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}
