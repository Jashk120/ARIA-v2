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

/// Run one read-only daemon query on demand (`query_budget`, `query_holds`,
/// `query_allowlist`, `query_wallet_balance`, `query_payment_history`,
/// `query_url_allowlist`) for the dashboard/history/settings panels. Every
/// one of these carries no fields beyond the query name itself, so they all
/// share this one thin wrapper — `query_url_rate_status` does not, since it
/// needs a `url` field, and gets its own command below. Each call is its
/// own TCP round trip — there is no background polling; panels call this
/// once on mount and again whenever the user hits refresh.
#[tauri::command]
async fn dashboard_query(query: String) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || daemon::send_query(&query))
        .await
        .map_err(|e| format!("Block thread error: {e}"))?
}

/// Add or remove an account on the payment allowlist via the daemon's
/// `mutate_allowlist` TCP endpoint. Returns the daemon's response verbatim
/// (`{ agent_did, action, account, changed }`) so the caller can tell a
/// real change from a no-op (e.g. adding an already-present account).
/// The Settings panel re-queries `query_allowlist` after this succeeds
/// rather than trusting this response to update its list — this call only
/// reports what the mutation itself did.
#[tauri::command]
async fn mutate_allowlist(action: String, account: String) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || daemon::mutate_allowlist(&action, &account))
        .await
        .map_err(|e| format!("Block thread error: {e}"))?
}

/// Approve a pending hold via the daemon's `approve_hold` TCP endpoint,
/// converting it into a committed payment. Returns the daemon's response
/// verbatim — the caller re-queries `query_holds`/`query_payment_history`
/// afterward rather than trusting this response to update local state.
#[tauri::command]
async fn approve_hold(
    app: AppHandle,
    state: State<'_, AppState>,
    payment_key: String,
) -> Result<Value, String> {
    let app_clone = app.clone();
    let res = tokio::task::spawn_blocking(move || {
        daemon::approve_hold(&payment_key, |event| {
            let mut final_result = String::new();
            let mut is_terminal_answer = false;
            let mut is_awaiting_confirmation = false;
            crate::agent::forward_daemon_event(&app_clone, event, &mut final_result, &mut is_terminal_answer, &mut is_awaiting_confirmation);
        })
    })
    .await
    .map_err(|e| format!("Block thread error: {e}"))??;
    let _ = state.db.clear_all_pending_confirmations();

    let final_res_str = res.to_string();
    crate::agent::FrontendEvent::DaemonDone {
        result: final_res_str,
        turn_done: true,
    }
    .emit(&app);

    Ok(res)
}

/// Release a pending hold via the daemon's `release_hold` TCP endpoint
/// without paying it. Same re-query-after pattern as `approve_hold`.
#[tauri::command]
async fn release_hold(
    app: AppHandle,
    state: State<'_, AppState>,
    payment_key: String,
) -> Result<Value, String> {
    let app_clone = app.clone();
    let res = tokio::task::spawn_blocking(move || {
        daemon::release_hold(&payment_key, |event| {
            let mut final_result = String::new();
            let mut is_terminal_answer = false;
            let mut is_awaiting_confirmation = false;
            crate::agent::forward_daemon_event(&app_clone, event, &mut final_result, &mut is_terminal_answer, &mut is_awaiting_confirmation);
        })
    })
    .await
    .map_err(|e| format!("Block thread error: {e}"))??;
    let _ = state.db.clear_all_pending_confirmations();

    let final_res_str = res.to_string();
    crate::agent::FrontendEvent::DaemonDone {
        result: final_res_str,
        turn_done: true,
    }
    .emit(&app);

    Ok(res)
}

/// Add or remove a URL on the x402 URL allowlist via the daemon's
/// `mutate_url_allowlist` TCP endpoint. This is a distinct mechanism from
/// `mutate_allowlist` above (which governs hedera_pay account recipients) —
/// kept as its own command rather than folded into it so the two never get
/// conflated on the wire or in the Settings UI. The caller re-queries
/// `query_url_allowlist` afterward rather than trusting this response to
/// update local state.
#[tauri::command]
async fn mutate_url_allowlist(action: String, url: String) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || daemon::mutate_url_allowlist(&action, &url))
        .await
        .map_err(|e| format!("Block thread error: {e}"))?
}

/// Fetch the current rate-limit status for one URL on the x402 allowlist
/// via the daemon's `query_url_rate_status` TCP endpoint (e.g. "7/10 this
/// hour"). Scoped to a single URL, unlike the other dashboard queries, so
/// it isn't folded into the generic `dashboard_query` command.
#[tauri::command]
async fn query_url_rate_status(url: String) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || daemon::query_url_rate_status(&url))
        .await
        .map_err(|e| format!("Block thread error: {e}"))?
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

#[tauri::command]
async fn clear_pending_confirmation(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    state
        .db
        .clear_pending_confirmation(&session_id)
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
            dashboard_query,
            mutate_allowlist,
            approve_hold,
            release_hold,
            mutate_url_allowlist,
            query_url_rate_status,
            agent::resume_daemon_task,
            create_session,
            save_message,
            save_event,
            load_messages,
            save_pending_confirmation,
            load_pending_confirmation,
            clear_pending_confirmation,
            list_sessions,
            delete_session
        ])
        .run(tauri::generate_context!())
        .expect("error while running ARIA GUI");
}