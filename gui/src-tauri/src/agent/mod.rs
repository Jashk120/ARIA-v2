use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::daemon;
use crate::llm::{self, ChatMessage, LlmStreamResult};

// ── Tauri Event Payloads ──────────────────────────────────────────────────────

/// Events emitted to the Svelte frontend via Tauri's event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrontendEvent {
    /// A streamed text token from the GUI LLM
    Token { content: String },
    /// The LLM finished a normal text response
    Done { full_text: String },
    /// The LLM is about to delegate a task to the daemon
    DaemonStarted { task: String, skill_type: String },
    /// An event forwarded from the daemon (thought, action, observation, final, etc.)
    DaemonEvent { event_type: String, payload: Value },
    /// The daemon finished executing a task (with the final content if any)
    DaemonDone { result: String },
    /// An error occurred at any stage
    Error { message: String },
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
                tokio::task::spawn_blocking(move || run_daemon_task(app_daemon, task, skill_type))
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

// ── Daemon Task Runner (blocking) ─────────────────────────────────────────────

/// Called on a blocking thread. Connects to the daemon, streams events back
/// to the frontend, and returns the final result string when done.
fn run_daemon_task(
    app: AppHandle,
    task: String,
    skill_type: String,
) -> (Result<(), String>, String) {
    let mut final_result = String::new();

    let res = daemon::submit_task(&task, &skill_type, |event| {
        let ev_type = event.event_type.clone();

        // Capture the final/chat content to return as the tool observation
        if ev_type == "final" || ev_type == "chat" {
            if let Some(content) = event.payload["content"].as_str() {
                final_result = content.to_string();
            }
        }

        FrontendEvent::DaemonEvent {
            event_type: ev_type,
            payload: event.payload,
        }
        .emit(&app);
    });

    (res, final_result)
}
