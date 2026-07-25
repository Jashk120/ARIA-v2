use reqwest::Client;
use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::daemon;
use crate::AppState;
use crate::llm::{self, ChatMessage, LlmStreamResult};

// ── Tauri Event Payloads ──────────────────────────────────────────────────────

/// Events emitted to the Svelte frontend via Tauri's event bus.
#[derive(Debug, Clone)]
pub enum FrontendEvent {
    /// A streamed text token from the GUI LLM
    Token { content: String },
    /// The LLM finished a normal text response
    Done { full_text: String },
    /// The LLM is about to delegate a task to the daemon
    DaemonStarted { task: String, skill_type: String },
    AwaitingConfirmation { task_id: String, content: String, kind: Option<String> },
    /// An event forwarded from the daemon (thought, action, observation, final, etc.)
    DaemonEvent { event_type: String, payload: Value },
    /// The daemon finished executing a task (with the final content if any)
    DaemonDone { result: String },
    /// An error occurred at any stage
    Error { message: String },
}

impl Serialize for FrontendEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            FrontendEvent::Token { content } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "token")?;
                map.serialize_entry("content", content)?;
                map.end()
            }
            FrontendEvent::Done { full_text } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "done")?;
                map.serialize_entry("full_text", full_text)?;
                map.end()
            }
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
            FrontendEvent::DaemonDone { result } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "daemon_done")?;
                map.serialize_entry("result", result)?;
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

/// Process one user turn through the agent loop.
/// Streams tokens/events to the frontend via Tauri events.
pub async fn run_turn(app: AppHandle, history: Vec<ChatMessage>) -> Result<(), String> {
    let client = Client::new();

    // Clone app handle for the token streaming closure
    let app_token = app.clone();
    let result = llm::stream_chat(&client, &history, move |token| {
        FrontendEvent::Token { content: token }.emit(&app_token);
    })
    .await;

    match result {
        // ── Normal text response — nothing else to do ─────────────────────
        Ok(LlmStreamResult::TextDone { full_text }) => {
            FrontendEvent::Done { full_text }.emit(&app);
        }

        // ── Tool call — delegate to the daemon ────────────────────────────
        Ok(LlmStreamResult::ToolCall { id: _, name, arguments }) => {
            if name != "delegate_to_daemon" {
                FrontendEvent::Error {
                    message: format!("Unsupported tool: {name}"),
                }
                .emit(&app);
                return Ok(());
            }

            // Parse the tool arguments JSON
            let args: Value = serde_json::from_str(&arguments)
                .map_err(|e| format!("Bad tool args JSON: {e}"))?;

            let task = args["task"]
                .as_str()
                .unwrap_or("unknown task")
                .to_string();
            let skill_type = args["type"].as_str().unwrap_or("fs").to_string();

            FrontendEvent::DaemonStarted {
                task: task.clone(),
                skill_type: skill_type.clone(),
            }
            .emit(&app);

            // TcpStream is synchronous — run it in a blocking thread pool
            let app_daemon = app.clone();
            let (res, final_result) =
                tokio::task::spawn_blocking(move || run_daemon_task(app_daemon, task, skill_type, None))
                    .await
                    .map_err(|e| format!("Block thread error: {e}"))?;

            if let Err(e) = res {
                FrontendEvent::Error { message: e }.emit(&app);
                return Ok(());
            }

            FrontendEvent::DaemonDone { result: final_result }.emit(&app);
        }

        Err(e) => {
            FrontendEvent::Error { message: e }.emit(&app);
        }
    }

    Ok(())
}

#[tauri::command]
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

    let (res, final_result) = tokio::task::spawn_blocking({
        let app_daemon = app.clone();
        let task_id = task_id.clone();
        move || run_daemon_task(app_daemon, reply, skill_type, Some(task_id))
    })
    .await
    .map_err(|e| format!("Block thread error: {e}"))?;

    if let Err(e) = res {
        FrontendEvent::Error { message: e }.emit(&app);
        return Ok(());
    }

    FrontendEvent::DaemonDone { result: final_result }.emit(&app);
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
) -> (Result<(), String>, String) {
    let mut final_result = String::new();

    let res = daemon::submit_task(&task, &skill_type, task_id, |event| {
        forward_daemon_event(&app, event, &mut final_result);
    });

    (res, final_result)
}

fn forward_daemon_event(app: &AppHandle, event: daemon::DaemonEvent, final_result: &mut String) {
    let ev_type = event.event_type.clone();

    if ev_type == "ask" {
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

    // Capture the final/chat content to return as the tool observation
    if ev_type == "final" || ev_type == "chat" {
        if let Some(content) = event.payload["content"].as_str() {
            *final_result = content.to_string();
        }
    }

    FrontendEvent::DaemonEvent {
        event_type: ev_type,
        payload: event.payload,
    }
    .emit(app);
}
