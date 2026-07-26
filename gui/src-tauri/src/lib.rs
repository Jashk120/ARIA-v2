mod agent;
mod daemon;
mod db;
mod llm;

use db::{Database, PendingConfirmation, StoredMessage};
use llm::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

// ── App State ─────────────────────────────────────────────────────────────────

pub struct AppState {
    pub db: Arc<Database>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DirectDaemonEvent {
    Started { task: String, skill_type: String },
    Event { event_type: String, payload: Value },
    Done,
    Error { message: String },
}

// ── Tauri Commands ────────────────────────────────────────────────────────────

/// Check if the ARIA daemon TCP socket is reachable.
#[tauri::command]
async fn check_daemon() -> bool {
    tokio::task::spawn_blocking(daemon::ping)
        .await
        .unwrap_or(false)
}

/// Process one user turn through the LLM agent loop.
#[tauri::command]
async fn send_message(app: AppHandle, history: Vec<ChatMessage>) -> Result<(), String> {
    agent::run_turn(app, history).await
}

/// Send one task directly to the daemon TCP socket without involving the chatbot.
#[tauri::command]
async fn send_direct_task(app: AppHandle, task: String, skill_type: String) -> Result<(), String> {
    let task = task.trim().to_string();
    let skill_type = skill_type.trim().to_string();

    if task.is_empty() {
        return Err("Task cannot be empty".to_string());
    }

    if skill_type.is_empty() {
        return Err("Type cannot be empty".to_string());
    }

    app.emit(
        "direct-daemon-event",
        DirectDaemonEvent::Started {
            task: task.clone(),
            skill_type: skill_type.clone(),
        },
    )
    .ok();

    let app_events = app.clone();
    let result = tokio::task::spawn_blocking(move || {
        daemon::submit_task(&task, &skill_type, None, |event| {
            app_events
                .emit(
                    "direct-daemon-event",
                    DirectDaemonEvent::Event {
                        event_type: event.event_type,
                        payload: event.payload,
                    },
                )
                .ok();
        })
    })
    .await
    .map_err(|e| format!("Block thread error: {e}"))?;

    match result {
        Ok(()) => {
            app.emit("direct-daemon-event", DirectDaemonEvent::Done).ok();
            Ok(())
        }
        Err(message) => {
            Err(message)
        }
    }
}

/// Ensure a session exists in the DB (idempotent – safe to call every load).
#[tauri::command]
async fn create_session(
    state: State<'_, AppState>,
    session_id: String,
    title: String,
) -> Result<(), String> {
    state
        .db
        .create_session(&session_id, &title)
        .map_err(|e| e.to_string())
}

/// Save a plain-text message to the database (back-compat; prefer save_event).
#[tauri::command]
async fn save_message(
    state: State<'_, AppState>,
    session_id: String,
    role: String,
    content: String,
) -> Result<i64, String> {
    state
        .db
        .save_message(&session_id, &role, &content)
        .map_err(|e| e.to_string())
}

/// Save any event (daemon thought/action/observation/final/chat, an ask,
/// an error) so it survives a reload instead of only living in the
/// in-memory UI state.
#[tauri::command]
async fn save_event(
    state: State<'_, AppState>,
    session_id: String,
    role: String,
    content: String,
    event_type: String,
    payload_json: Option<String>,
    group_id: Option<String>,
) -> Result<i64, String> {
    state
        .db
        .save_event(
            &session_id,
            &role,
            &content,
            &event_type,
            payload_json.as_deref(),
            group_id.as_deref(),
        )
        .map_err(|e| e.to_string())
}

/// Load all messages for a session.
#[tauri::command]
async fn load_messages(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<StoredMessage>, String> {
    state
        .db
        .load_messages(&session_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn save_pending_confirmation(
    state: State<'_, AppState>,
    session_id: String,
    task_id: String,
    content: String,
    kind: Option<String>,
    skill_type: String,
) -> Result<(), String> {
    state
        .db
        .save_pending_confirmation(
            &session_id,
            &task_id,
            &content,
            kind.as_deref(),
            &skill_type,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn load_pending_confirmation(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<PendingConfirmation>, String> {
    state
        .db
        .load_pending_confirmation(&session_id)
        .map_err(|e| e.to_string())
}

/// List all sessions.
#[tauri::command]
async fn list_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<(String, String, i64)>, String> {
    state.db.list_sessions().map_err(|e| e.to_string())
}

/// Delete a session and its messages.
#[tauri::command]
async fn delete_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    state
        .db
        .delete_session(&session_id)
        .map_err(|e| e.to_string())
}

// ── App Entry Point ───────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Resolve the data directory for the current platform
            let mut db_path = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data dir");
            std::fs::create_dir_all(&db_path).ok();
            db_path.push("aria.db");

            let db = Database::open(db_path).expect("failed to open database");
            app.manage(AppState { db: Arc::new(db) });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            check_daemon,
            send_message,
            send_direct_task,
            agent::resume_daemon_task,
            create_session,
            save_message,
            save_event,
            load_messages,
            save_pending_confirmation,
            load_pending_confirmation,
            list_sessions,
            delete_session
        ])
        .run(tauri::generate_context!())
        .expect("error while running ARIA GUI");
}
