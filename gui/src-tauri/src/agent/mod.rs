use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::daemon;
use crate::AppState;
// NOTE: the GUI owns no LLM — it is a pure daemon pass-through.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
}

// ── Tauri Event Payloads ──────────────────────────────────────────────────────

/// Events emitted to the Svelte frontend via Tauri's event bus.
#[derive(Debug, Clone)]
pub enum FrontendEvent {
    /// The GUI is about to delegate a task to the daemon
    DaemonStarted { task: String, skill_type: String },
    AwaitingConfirmation { task_id: String, content: String, kind: Option<String> },
    /// An event forwarded from the daemon (thought, action, observation, final, etc.)
    DaemonEvent { event_type: String, payload: Value },
    /// The daemon finished executing a task (with the final content if any)
    DaemonDone { result: String, turn_done: bool },
    /// An error occurred at any stage
    Error { message: String },
}

impl Serialize for FrontendEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            FrontendEvent::DaemonStarted { task, skill_type } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "daemon_started")?;
                map.serialize_entry("task", task)?;
                map.serialize_entry("skill_type", skill_type)?;
                map.end()
            }
            FrontendEvent::AwaitingConfirmation {
                task_id,
                content,
                kind,
            } => {
                let mut map = serializer.serialize_map(Some(if kind.is_some() { 4 } else { 3 }))?;
                map.serialize_entry("kind", "awaiting_confirmation")?;
                map.serialize_entry("task_id", task_id)?;
                map.serialize_entry("content", content)?;
                if let Some(kind) = kind {
                    let payload = serde_json::json!({ "kind": kind });
                    map.serialize_entry("payload", &payload)?;
                }
                map.end()
            }
            FrontendEvent::DaemonEvent {
                event_type,
                payload,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "daemon_event")?;
                map.serialize_entry("event_type", event_type)?;
                map.serialize_entry("payload", payload)?;
                map.end()
            }
            FrontendEvent::DaemonDone { result, turn_done } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "daemon_done")?;
                map.serialize_entry("result", result)?;
                map.serialize_entry("turn_done", turn_done)?;
                map.end()
            }
            FrontendEvent::Error { message } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "error")?;
                map.serialize_entry("message", message)?;
                map.end()
            }
        }
    }
}

impl FrontendEvent {
    pub fn emit(self, app: &AppHandle) {
        let _ = app.emit("aria-event", self);
    }
}

// ── Agent: Single Turn ────────────────────────────────────────────────────────

/// Process one user turn as a pure pass-through to the daemon.
///
/// The GUI owns NO router LLM: the latest user message from `history` goes
/// straight to `run_daemon_task` (daemon `submit_task` over TCP) on a
/// blocking thread, daemon events stream to the frontend via
/// `forward_daemon_event`, and the daemon's terminal answer is delivered
/// exactly once via `DaemonDone`. Exactly one writer closes the turn:
/// `Error` on transport failure, otherwise a single `DaemonDone`.
/// An `awaiting_confirmation` turn emits no `DaemonDone` — the confirmation
/// card was already emitted and `resume_daemon_task` drives what happens next.
///
/// The prior transcript is forwarded alongside the latest message so the
/// daemon can seed the loop with conversation context; without it every turn
/// is a fresh, memoryless task.
pub async fn run_turn(app: AppHandle, history: Vec<ChatMessage>) -> Result<(), String> {
    let task = history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
        .trim()
        .to_string();

    if task.is_empty() {
        FrontendEvent::Error {
            message: "Empty message".to_string(),
        }
        .emit(&app);
        return Ok(());
    }

    let prior_turns = wire_history(&history);
    let skill_type = "fs".to_string();

    FrontendEvent::DaemonStarted {
        task: task.clone(),
        skill_type: skill_type.clone(),
    }
    .emit(&app);

    // TcpStream is synchronous — run it in a blocking thread pool
    let app_daemon = app.clone();
    let (res, final_result, _daemon_gave_final_answer, awaiting_confirmation) =
        tokio::task::spawn_blocking(move || {
            run_daemon_task(app_daemon, task, skill_type, None, prior_turns)
        })
        .await
        .map_err(|e| format!("Block thread error: {e}"))?;

    if let Err(e) = res {
        FrontendEvent::Error { message: e }.emit(&app);
        return Ok(());
    }

    // The daemon paused this task on a human confirmation (e.g. a
    // payment above the auto-approval threshold). AwaitingConfirmation
    // was already emitted straight to the frontend, which renders the
    // Yes/No card. Stop the turn here. `resumeInlineAsk` on the frontend
    // drives what happens next, via `resume_daemon_task`.
    if awaiting_confirmation {
        return Ok(());
    }

    FrontendEvent::DaemonDone { result: final_result, turn_done: true }.emit(&app);
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn resume_daemon_task(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    task_id: String,
    reply: String,
    skill_type: String,
) -> Result<(), String> {
    state
        .db
        .clear_pending_confirmation(&session_id)
        .map_err(|e| e.to_string())?;

    let (res, final_result, _daemon_gave_final_answer, _awaiting_confirmation) = tokio::task::spawn_blocking({
        let app_daemon = app.clone();
        let task_id = task_id.clone();
        move || run_daemon_task(app_daemon, reply, skill_type, Some(task_id), None)
    })
    .await
    .map_err(|e| format!("Block thread error: {e}"))?;

    if let Err(e) = res {
        FrontendEvent::Error { message: e }.emit(&app);
        return Ok(());
    }

    FrontendEvent::DaemonDone { result: final_result, turn_done: true }.emit(&app);
    Ok(())
}

// ── Daemon Task Runner (blocking) ─────────────────────────────────────────────

/// Called on a blocking thread. Connects to the daemon, streams events back
/// to the frontend, and returns the final result string when done.
fn run_daemon_task(
    app: AppHandle,
    task: String,
    skill_type: String,
    task_id: Option<String>,
    history: Option<Vec<daemon::HistoryTurn>>,
) -> (Result<(), String>, String, bool, bool) {
    let mut final_result = String::new();
    let mut is_terminal_answer = false;
    let mut is_awaiting_confirmation = false;

    let res = daemon::submit_task(&task, &skill_type, task_id, history, |event| {
        forward_daemon_event(&app, event, &mut final_result, &mut is_terminal_answer, &mut is_awaiting_confirmation);
    });

    (res, final_result, is_terminal_answer, is_awaiting_confirmation)
}

/// Max prior turns forwarded to the daemon, bounding payload size and prompt
/// cost. The daemon's request reader tolerates up to 1 MiB.
const MAX_HISTORY_TURNS: usize = 40;

/// Map the frontend transcript into wire turns, dropping non-user/assistant
/// or empty lines, then keeping only the most recent `MAX_HISTORY_TURNS`.
fn wire_history(history: &[ChatMessage]) -> Option<Vec<daemon::HistoryTurn>> {
    let mut turns: Vec<daemon::HistoryTurn> = history
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .filter_map(|m| {
            let content = m.content.as_deref().unwrap_or("").trim();
            if content.is_empty() {
                None
            } else {
                Some(daemon::HistoryTurn {
                    role: m.role.clone(),
                    content: content.to_string(),
                })
            }
        })
        .collect();

    if turns.len() > MAX_HISTORY_TURNS {
        turns.drain(0..turns.len() - MAX_HISTORY_TURNS);
    }

    if turns.is_empty() { None } else { Some(turns) }
}

/// Forwards one daemon event to the frontend. `final_result` accumulates the
/// content of the last `final`/`chat` event so the caller can deliver it
/// exactly once via `DaemonDone` (the frontend appends it to the daemon
/// block there). `is_terminal_answer` is set to `true` whenever a
/// `final`/`chat` event is seen. `is_awaiting_confirmation` is set to `true`
/// when the daemon paused on an `ask` — a signal that this task has NOT
/// finished.
/// Terminal `final`/`chat` events are NOT forwarded as `DaemonEvent`: they
/// are already delivered via `DaemonDone`, and forwarding both would render
/// the daemon's answer twice.
pub fn forward_daemon_event(
    app: &AppHandle,
    event: daemon::DaemonEvent,
    final_result: &mut String,
    is_terminal_answer: &mut bool,
    is_awaiting_confirmation: &mut bool,
) {
    let ev_type = event.event_type.clone();

    if ev_type == "ask" {
        *is_awaiting_confirmation = true;
        if let (Some(task_id), Some(content)) = (
            event.payload["task_id"].as_str(),
            event.payload["content"].as_str(),
        ) {
            FrontendEvent::AwaitingConfirmation {
                task_id: task_id.to_string(),
                content: content.to_string(),
                kind: event.payload["kind"].as_str().map(str::to_string),
            }
            .emit(app);
        }
        return;
    }

    if ev_type == "final" || ev_type == "chat" {
        if let Some(content) = event.payload["content"].as_str() {
            *final_result = content.to_string();
            *is_terminal_answer = true;
        }
        // Delivered exactly once via DaemonDone — do NOT also emit DaemonEvent.
        return;
    }

    FrontendEvent::DaemonEvent { event_type: ev_type, payload: event.payload }.emit(app);
}

#[cfg(test)]
mod wire_history_tests {
    use super::{wire_history, ChatMessage, MAX_HISTORY_TURNS};

    fn msg(role: &str, content: Option<&str>) -> ChatMessage {
        ChatMessage { role: role.to_string(), content: content.map(str::to_string) }
    }

    #[test]
    fn drops_non_conversational_and_empty() {
        let hist = [msg("system", Some("x")), msg("user", Some("  ")), msg("assistant", Some("hi"))];
        let out = wire_history(&hist).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "assistant");
        assert_eq!(out[0].content, "hi");
    }

    #[test]
    fn none_when_nothing_usable() {
        assert!(wire_history(&[]).is_none());
        assert!(wire_history(&[msg("user", None)]).is_none());
    }

    #[test]
    fn caps_to_most_recent() {
        let hist: Vec<ChatMessage> = (0..50).map(|i| msg("user", Some(&format!("m{i}")))).collect();
        let out = wire_history(&hist).unwrap();
        assert_eq!(out.len(), MAX_HISTORY_TURNS);
        assert_eq!(out[0].content, "m10");
    }

    #[test]
    fn trims_content() {
        let out = wire_history(&[msg("user", Some("  hello  "))]).unwrap();
        assert_eq!(out[0].content, "hello");
    }
}
