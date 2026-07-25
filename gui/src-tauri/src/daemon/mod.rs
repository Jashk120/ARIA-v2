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
