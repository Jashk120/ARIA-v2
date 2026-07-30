use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

const DAEMON_ADDR: &str = "127.0.0.1:5005";
const MAX_RETRIES: u32 = 4;
const BASE_RETRY_MS: u64 = 300;

// ── Daemon Wire Types ─────────────────────────────────────────────────────────

/// The JSON request we send to the daemon over TCP.
#[derive(Debug, Serialize)]
pub struct DaemonRequest {
    pub task: String,
    #[serde(rename = "Type")]
    pub skill_type: String,
    pub task_id: Option<String>,
}

/// Every line the daemon sends back is a `DaemonEvent`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(flatten)]
    pub payload: Value,
}

// ── Retry Connect ─────────────────────────────────────────────────────────────

/// Quick check: returns true if the daemon is reachable.
pub fn ping() -> bool {
    TcpStream::connect_timeout(
        &DAEMON_ADDR.parse().unwrap(),
        Duration::from_millis(500),
    )
    .is_ok()
}


fn connect_with_retries() -> Result<TcpStream, String> {
    let mut delay = BASE_RETRY_MS;
    for attempt in 1..=MAX_RETRIES {
        match TcpStream::connect(DAEMON_ADDR) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(120)))
                    .ok();
                return Ok(stream);
            }
            Err(e) => {
                if attempt == MAX_RETRIES {
                    return Err(format!(
                        "Daemon unreachable after {MAX_RETRIES} attempts: {e}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(delay));
                delay = (delay * 2).min(5000);
            }
        }
    }
    unreachable!()
}

// ── Read-Only Queries ─────────────────────────────────────────────────────────

/// The JSON request for a read-only daemon query (`query_budget`,
/// `query_holds`, `query_allowlist`, `query_wallet_balance`). Distinct from
/// `DaemonRequest` above since these carry no `task`/`Type` — the daemon
/// short-circuits on the presence of `query` before touching the ReAct loop.
#[derive(Debug, Serialize)]
struct DaemonQueryRequest<'a> {
    query: &'a str,
}

/// Serializes `payload`, sends it as a single line over a fresh TCP
/// connection, and returns the one-line JSON response. Shared plumbing for
/// every single-shot query/mutation endpoint below (`send_query`,
/// `mutate_allowlist`, and everything added since) — the daemon closes the
/// socket after the one line, so there's nothing to keep alive here.
fn send_request<T: Serialize>(payload: &T) -> Result<Value, String> {
    let stream = connect_with_retries()?;
    let mut write_stream = stream.try_clone().map_err(|e| e.to_string())?;

    let request_json =
        serde_json::to_string(payload).map_err(|e| format!("Serialization error: {e}"))?;

    write_stream
        .write_all(request_json.as_bytes())
        .map_err(|e| format!("Write error: {e}"))?;
    write_stream
        .write_all(b"\n")
        .map_err(|e| format!("Write error: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("Read error: {e}"))?;
    let line = line.trim();
    if line.is_empty() {
        return Err("Empty response from daemon".to_string());
    }

    let value: Value =
        serde_json::from_str(line).map_err(|e| format!("Parse error on '{line}': {e}"))?;

    if value.get("type").and_then(|t| t.as_str()) == Some("query_error") {
        let message =
            value.get("message").and_then(|m| m.as_str()).unwrap_or("unknown daemon error");
        return Err(message.to_string());
    }

    Ok(value)
}

/// Sends a single read-only query and returns the one-line JSON response the
/// daemon writes back (tagged `{"type": "query_budget" | "query_holds" |
/// "query_allowlist" | "query_wallet_balance" | "query_payment_history" |
/// "query_url_allowlist" | "query_error", ...}`). Unlike `submit_task`, this
/// is a single request/response round trip, not an event stream — the
/// daemon closes the socket after the one line.
pub fn send_query(query: &str) -> Result<Value, String> {
    send_request(&DaemonQueryRequest { query })
}

// ── Allowlist Mutation ────────────────────────────────────────────────────────

/// The JSON request for the `mutate_allowlist` daemon endpoint. Distinct from
/// `DaemonQueryRequest` since this carries `mutate`/`action`/`account` instead
/// of `query` — the daemon short-circuits on the presence of `mutate` the
/// same way it does for `query`, before touching the ReAct loop.
#[derive(Debug, Serialize)]
struct DaemonMutateRequest<'a> {
    mutate: &'a str,
    action: &'a str,
    account: &'a str,
}

/// Sends a single `mutate_allowlist` request ("add" or "remove") and returns
/// the one-line JSON response the daemon writes back (tagged
/// `{"type": "mutate_allowlist", "action", "account", "changed"}` on
/// success, or `{"type": "query_error", "message"}` on failure). Like
/// `send_query`, this is a single request/response round trip — the daemon
/// closes the socket after the one line.
pub fn mutate_allowlist(action: &str, account: &str) -> Result<Value, String> {
    send_request(&DaemonMutateRequest {
        mutate: "mutate_allowlist",
        action,
        account,
    })
}

// ── URL Allowlist (x402) ──────────────────────────────────────────────────────
//
// Distinct from the account allowlist above: this one governs which URLs
// x402_pay is allowed to autonomously pay, not which Hedera accounts
// hedera_pay may send to. Kept as its own request/response shape rather
// than reusing `DaemonMutateRequest`'s `account` field, since conflating
// the two wire formats would blur the same distinction the GUI is meant to
// keep visually explicit.

/// The JSON request for the `mutate_url_allowlist` daemon endpoint.
#[derive(Debug, Serialize)]
struct UrlAllowlistMutateRequest<'a> {
    mutate: &'a str,
    action: &'a str,
    url: &'a str,
}

/// Sends a single `mutate_url_allowlist` request ("add" or "remove") and
/// returns the one-line JSON response the daemon writes back (tagged
/// `{"type": "mutate_url_allowlist", "action", "url", "changed"}` on
/// success). Same request/response round trip shape as `mutate_allowlist`.
pub fn mutate_url_allowlist(action: &str, url: &str) -> Result<Value, String> {
    send_request(&UrlAllowlistMutateRequest {
        mutate: "mutate_url_allowlist",
        action,
        url,
    })
}

/// The JSON request for the `query_url_rate_status` daemon endpoint —
/// unlike the other read-only queries, this one is scoped to a single URL
/// rather than returning the whole allowlist, so it carries a `url` field
/// alongside `query`.
#[derive(Debug, Serialize)]
struct UrlRateStatusRequest<'a> {
    query: &'a str,
    url: &'a str,
}

/// Sends a `query_url_rate_status` request for one URL and returns the
/// one-line JSON response (tagged `{"type": "query_url_rate_status", "url",
/// ...}`, expected to carry the URL's current request count against its
/// rate-limit window). Called once per listed URL from the Settings panel.
pub fn query_url_rate_status(url: &str) -> Result<Value, String> {
    send_request(&UrlRateStatusRequest {
        query: "query_url_rate_status",
        url,
    })
}

// ── Hold Mutations (release_hold / approve_hold) ──────────────────────────────

/// The JSON request for the `release_hold` / `approve_hold` daemon
/// endpoints. Both act on a single outstanding hold, identified the same
/// way `query_holds` identifies one in its response — by `payment_key`.
#[derive(Debug, Serialize)]
struct HoldMutateRequest<'a> {
    mutate: &'a str,
    payment_key: &'a str,
}

fn send_hold_mutate_request<F>(
    mutate_type: &str,
    payment_key: &str,
    mut on_event: F,
) -> Result<Value, String>
where
    F: FnMut(DaemonEvent),
{
    let stream = connect_with_retries()?;
    let mut write_stream = stream.try_clone().map_err(|e| e.to_string())?;

    let request = HoldMutateRequest {
        mutate: mutate_type,
        payment_key,
    };

    let request_json =
        serde_json::to_string(&request).map_err(|e| format!("Serialization error: {e}"))?;

    write_stream
        .write_all(request_json.as_bytes())
        .map_err(|e| format!("Write error: {e}"))?;
    write_stream
        .write_all(b"\n")
        .map_err(|e| format!("Write error: {e}"))?;

    let reader = BufReader::new(stream);
    let mut last_value = Value::Null;
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Read error: {e}"))?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let event: Value = serde_json::from_str(&line)
            .map_err(|e| format!("Parse error on '{line}': {e}"))?;

        if let Some(event_type) = event.get("type").and_then(|t| t.as_str()) {
            if event_type == "mutate_hold" {
                last_value = event;
                break;
            } else if event_type == "query_error" {
                let message = event.get("message").and_then(|m| m.as_str()).unwrap_or("unknown daemon error");
                return Err(message.to_string());
            } else {
                if let Ok(daemon_event) = serde_json::from_value::<DaemonEvent>(event) {
                    on_event(daemon_event);
                }
            }
        }
    }

    Ok(last_value)
}

/// Approves a pending hold, releasing it into a committed payment. Returns
/// the daemon's response verbatim; the caller re-queries `query_holds` (and
/// `query_payment_history`) afterward rather than trusting this response to
/// update local state.
pub fn approve_hold<F>(payment_key: &str, on_event: F) -> Result<Value, String>
where
    F: FnMut(DaemonEvent),
{
    send_hold_mutate_request("approve_hold", payment_key, on_event)
}

/// Releases a pending hold without paying it — the funds return to
/// available budget. Same re-query-after pattern as `approve_hold`.
pub fn release_hold<F>(payment_key: &str, on_event: F) -> Result<Value, String>
where
    F: FnMut(DaemonEvent),
{
    send_hold_mutate_request("release_hold", payment_key, on_event)
}

// ── Task Submission ───────────────────────────────────────────────────────────


/// Connect to the daemon, submit a task, and iterate over the event stream.
/// Calls `on_event` for every `DaemonEvent` received until a `done` or `error` event.
pub fn submit_task<F>(
    task: &str,
    skill_type: &str,
    task_id: Option<String>,
    mut on_event: F,
) -> Result<(), String>
where
    F: FnMut(DaemonEvent),
{
    let stream = connect_with_retries()?;
    let mut write_stream = stream.try_clone().map_err(|e| e.to_string())?;

    let request = DaemonRequest {
        task: task.to_string(),
        skill_type: skill_type.to_string(),
        task_id,
    };

    let request_json =
        serde_json::to_string(&request).map_err(|e| format!("Serialization error: {e}"))?;

    write_stream
        .write_all(request_json.as_bytes())
        .map_err(|e| format!("Write error: {e}"))?;
    write_stream
        .write_all(b"\n")
        .map_err(|e| format!("Write error: {e}"))?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Read error: {e}"))?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let event: DaemonEvent = serde_json::from_str(&line)
            .map_err(|e| format!("Parse error on '{line}': {e}"))?;

        let is_terminal = event.event_type == "done" || event.event_type == "error";
        on_event(event);
        if is_terminal {
            break;
        }
    }

    Ok(())
}