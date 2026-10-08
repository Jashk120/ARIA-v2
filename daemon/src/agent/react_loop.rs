//! The ReAct execution loop: drives repeated LLM calls, parses structured
//! agent responses (thought/action/ask/final/chat), dispatches skills via
//! SkillManager, and enforces per-skill max_steps / terminal behavior from
//! each skill's manifest.
//!
//! One turn may dispatch up to MAX_PARALLEL_ACTIONS independent actions
//! concurrently (see `plan_action_batch` / `execute_parallel_batch`); the
//! MAX_REACT_STEPS budget counts turns, not individual tool calls.

use std::collections::HashMap;

use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use super::prompt::{
    build_native_tools_with_dlt,
    system_prompt_with_dlt,
};
use crate::config::{LlmSettings, Protocol};
use crate::payments::governance::compute_payment_key;
use crate::skills::manifest::{
    Capabilities,
    ReactConfig,
    load_manifest,
};
use crate::skills::paths::{
    skill_dir,
    wasm_path,
};

// MAX_REACT_STEPS counts turns, not tool calls: one turn may now contain up
// to MAX_PARALLEL_ACTIONS concurrent calls (tier 1 of the hierarchy plan —
// parallel tools, zero extra LLM calls).
pub const MAX_REACT_STEPS: usize = 8;

/// Max independent actions executed concurrently in one turn. The model may
/// emit up to this many `Action{skill,args}` per assistant message (native
/// tool calls) or as consecutive `{"type":"action",...}` lines
/// (prompt-fallback). Extras are deferred with a note observation, never
/// dropped silently and never run.
pub const MAX_PARALLEL_ACTIONS: usize = 5;

/// How long the background settlement watcher (see `spawn_payment_settlement_watch`)
/// keeps polling the mirror node before giving up on a transaction.
const PAYMENT_SETTLEMENT_MAX_POLLS: usize = 20;
const PAYMENT_SETTLEMENT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// If `transaction_id` is `Some` (i.e. the skill that just ran was a payment
/// skill that actually submitted), spawns a detached background task that
/// polls the mirror node until the transaction reaches a final state. The
/// result is always persisted to `payments.status` — so `query_payment_history`
/// and any caller with no live event stream (e.g. the TCP `approve_hold`
/// endpoint, which returns and closes before settlement) still see the final
/// state on their next read. When `tx` is available, the same result is also
/// pushed as `AgentEvent::PaymentSettled` so a connection still streaming
/// this task sees it live, without polling itself.
fn spawn_payment_settlement_watch(
    db: std::sync::Arc<crate::db::Db>,
    tx: Option<mpsc::Sender<AgentEvent>>,
    transaction_id: Option<String>,
) {
    let Some(transaction_id) = transaction_id else { return };
    tokio::spawn(async move {
        if let Some(status) = crate::payments::mirror::poll_until_final(
            &transaction_id,
            PAYMENT_SETTLEMENT_MAX_POLLS,
            PAYMENT_SETTLEMENT_POLL_INTERVAL,
        )
        .await
        {
            let _ = db.update_payment_status(&transaction_id, &status);
            if let Some(tx) = tx {
                let _ = tx.send(AgentEvent::PaymentSettled { transaction_id, status }).await;
            }
        }
    });
}

// ── Agent event / response types ──────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    Payment,
    SkillGrant,
    // Future: Clarification is intentionally NOT added as a variant.
    // Absence of `kind` already means "plain question" — don't encode
    // that case explicitly, or every clarification call site has to
    // remember to set it. Only add variants here as new *decision-
    // grade* Ask reasons emerge (e.g. a future access/permission
    // confirmation), following the same rule: only add a variant when
    // the surrounding Rust code deterministically knows the reason
    // without asking the LLM.
}

/// Span hierarchy contract (single-brain direction: daemon owns reasoning,
/// GUI is a thin renderer):
/// - `span_id` / `parent_span_id` / `depth` are ALWAYS optional and
///   `skip_serializing_if None`, so existing GUI parsing keeps working.
/// - Top-level turn events carry `depth: Some(0)` (or `None`, treated as 0)
///   with no parent. Nested execution detail (e.g. an Observation produced by
///   an Action) carries `depth > 0` with `parent_span_id` pointing at the
///   parent's `span_id`.
/// - The GUI renders `depth > 0` events collapsible under their parent span.
/// - NOTE: the serde `tag = "type", rename_all = "lowercase"` is unchanged;
///   `PaymentSettled` serializes as `"paymentsettled"` — a separate fix owns
///   that rename, do NOT touch it here.
#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AgentEvent {
    Thought {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depth: Option<u8>,
    },
    Action {
        skill: String,
        args: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depth: Option<u8>,
    },
    Observation {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depth: Option<u8>,
    },
    Ask {
        content: String,
        task_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<AskKind>,
    },
    /// Streamed token — print immediately, no newline
    Token {
        content: String,
    },
    /// Full assembled text after streaming completes (Chat or Final)
    Final {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depth: Option<u8>,
    },
    Chat {
        content: String,
    },
    Error {
        content: String,
    },
    /// Pushed by a background mirror-node poll once an in-flight payment
    /// transaction reaches a final on-chain state. Not sent inline with
    /// `Observation` — the skill call returns as soon as it *submits*, this
    /// arrives later, on the same connection, once it *settles*.
    PaymentSettled {
        transaction_id: String,
        status: String,
    },
    Done,
}

/// Short random hex span id for the event-stream span hierarchy (see the
/// contract comment on `AgentEvent`). Uses the existing `rand` dep — no new
/// dependencies. 4 bytes → 8 hex chars is enough to correlate a turn's
/// action/observation pair in the GUI.
pub(crate) fn new_span_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

pub(crate) enum AgentResponseKind {
    Chat(String),
    Thought(String),
    Action { skill: String, args: serde_json::Value },
    Ask(String),
    Final(String),
}

#[derive(Debug, PartialEq, Eq)]
enum ConfirmationDecision {
    Confirmed,
    Denied,
    ContinueConversation,
}

/// Releases a pending payment confirmation's spend hold and clears the
/// pending action — the exact effect of a human replying "no" to a payment
/// confirmation in chat. Shared by `ConfirmationDecision::Denied` below and
/// the TCP `release_hold` endpoint in `main.rs`, so the two code paths can
/// never diverge. Returns the "Cancelled" message text the caller should
/// surface (as an `AgentEvent::Final` in chat, or as the TCP response body).
pub fn release_hold(
    db: &crate::db::Db,
    agent_did: &str,
    task_id: &str,
    skill: &str,
    args: &serde_json::Value,
) -> String {
    if let Ok((rec, amt)) = extract_payment_recipient_and_amount(skill, args) {
        let pkey = compute_payment_key(agent_did, &rec, amt);
        let _ = db.release_spend_hold(agent_did, &pkey);
    }
    let _ = db.clear_pending_action(task_id);
    format!("Cancelled — {} was not executed.", skill)
}

/// Outcome of attempting to execute a pending payment confirmation — the
/// exact effect of a human replying "yes" in chat. Returned by
/// [`approve_hold`] so callers (the chat resume path below, and the TCP
/// `approve_hold` endpoint in `main.rs`) can react appropriately without
/// duplicating the underlying fingerprint-check / execute / commit-or-release
/// logic.
pub enum ApproveHoldOutcome {
    /// The pending action failed static validation (e.g. missing recipient)
    /// before anything executed. The hold was already released and the
    /// pending action already cleared.
    Invalid { message: String },
    /// The stored fingerprint no longer matches the current one (skill
    /// config changed since the hold was placed). The old hold was released
    /// and a fresh confirmation (`refreshed_pending`) is ready to be saved;
    /// nothing executed. Caller should surface `question` and ask again.
    FingerprintChanged { question: String, refreshed_pending: serde_json::Value },
    /// The skill actually ran. `tx_id` is `Some` when the skill's result
    /// included a `transaction_id` field, as every payment skill's output
    /// does on success.
    Executed { success: bool, observation: String, tx_id: Option<String> },
}

/// Executes a pending payment confirmation — the exact effect of a human
/// replying "yes" to a payment confirmation in chat: fingerprint check,
/// `run_skill_raw`, then commit the spend hold on success or release it on
/// failure. Shared by `ConfirmationDecision::Confirmed` below and the TCP
/// `approve_hold` endpoint in `main.rs`, so the two code paths can never
/// diverge.
///
/// `tx`, when provided, receives the same `AgentEvent::Action` chat already
/// emits right before executing — passed through so this stays a genuine
/// UI-side effect rather than something baked into the shared logic. The TCP
/// caller has no stream to emit to and passes `None`.
#[allow(clippy::too_many_arguments)]
pub async fn approve_hold(
    db: &std::sync::Arc<crate::db::Db>,
    skills: &std::sync::Arc<crate::skills::SkillManager>,
    payment_vault: Option<std::sync::Arc<crate::payments::direct::PaymentVault>>,
    x402_vault: Option<std::sync::Arc<crate::payments::x402_vault::X402PaymentVault>>,
    injected_config: &HashMap<String, HashMap<String, String>>,
    agent_did: &str,
    task_id: &str,
    skill: &str,
    args: &serde_json::Value,
    stored_fingerprint: &str,
    tx: Option<&mpsc::Sender<AgentEvent>>,
) -> ApproveHoldOutcome {
    if let Some(error) = payment_proposal_error(skill, args) {
        if let Ok((rec, amt)) = extract_payment_recipient_and_amount(skill, args) {
            let pkey = compute_payment_key(agent_did, &rec, amt);
            let _ = db.release_spend_hold(agent_did, &pkey);
        }
        let _ = db.clear_pending_action(task_id);
        return ApproveHoldOutcome::Invalid { message: error };
    }

    // Air-gap: a hold approved after the flag was flipped off must still
    // fail closed — same release + clear shape as the invalid-proposal path.
    if let Some(message) = dlt_blocked_error(db, skill) {
        if let Ok((rec, amt)) = extract_payment_recipient_and_amount(skill, args) {
            let pkey = compute_payment_key(agent_did, &rec, amt);
            let _ = db.release_spend_hold(agent_did, &pkey);
        }
        let _ = db.clear_pending_action(task_id);
        return ApproveHoldOutcome::Invalid { message };
    }

    let current_fingerprint = payment_fingerprint(skill, args, injected_config);
    if stored_fingerprint != current_fingerprint {
        if let Ok((rec, amt)) = extract_payment_recipient_and_amount(skill, args) {
            let old_pkey = compute_payment_key(agent_did, &rec, amt);
            let _ = db.release_spend_hold(agent_did, &old_pkey);
        }
        let refreshed = pending_payment_action(skill, args.clone(), injected_config);
        let question = payment_confirmation_message(skill, args);
        return ApproveHoldOutcome::FingerprintChanged { question, refreshed_pending: refreshed };
    }

    // Extract payment key now so we can commit/release after run_skill_raw.
    let payment_key_for_hold = extract_payment_recipient_and_amount(skill, args)
        .ok()
        .map(|(rec, amt)| compute_payment_key(agent_did, &rec, amt));

    let _ = db.clear_pending_action(task_id);

    if let Some(tx) = tx {
        let _ = tx
            .send(AgentEvent::Action {
                skill: skill.to_string(),
                args: args.clone(),
                span_id: Some(new_span_id()),
                parent_span_id: None,
                depth: Some(0),
            })
            .await;
    }

    // ── deterministic: no model calls past this point ──
    // The executor below (`run_skill_raw` + hold commit/release) is pure
    // deterministic dispatch: WASM execution, DB writes, and structured
    // error observations. Any LLM fallback/retry belongs in the ReAct loop
    // above, never here — on failure return the error as an observation.
    let mut enriched = args.clone();
    if let Some(obj) = enriched.as_object_mut()
        && let Some(skill_config) = injected_config.get(skill)
    {
        for (k, v) in skill_config {
            obj.insert(k.clone(), json!(v));
        }
    }

    let (observation, tx_id, is_error): (String, Option<String>, bool) = match skills
        .run_skill_raw(
            skill,
            &enriched,
            Some(db.clone()),
            payment_vault,
            x402_vault,
            agent_did.to_string(),
            Some(task_id.to_string()),
        )
        .await
    {
        Ok(val) => {
            let tx_id = val.get("transaction_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            (val.to_string(), tx_id, false)
        }
        Err(e) => (e.to_string(), None, true),
    };

    if let Some(ref pkey) = payment_key_for_hold {
        if is_error {
            let _ = db.release_spend_hold(agent_did, pkey);
        } else {
            let _ = db.commit_spend_hold(agent_did, pkey);
        }
    }

    if !is_error {
        spawn_payment_settlement_watch(db.clone(), tx.cloned(), tx_id.clone());
    }

    ApproveHoldOutcome::Executed { success: !is_error, observation, tx_id }
}

// ── Model Capability Allowlist ────────────────────────────────────────────────

fn static_capability(model: &str) -> Option<crate::db::ToolCapability> {
    let lower = model.to_lowercase();
    if lower.contains("claude")
        || lower.contains("gpt-")
        || lower.contains("gemini-1.5")
        || lower.contains("gemini-2")
        || lower.contains("gemini-3")
        || lower.contains("gemini-4")
        || lower.contains("gemma4")
        || lower.contains("gemma-4")
        || lower.contains("deepseek-v4")
        || lower.contains("qwen")
        || lower.contains("grok")
        || lower.contains("mimo")
        || lower.contains("glm")
        || lower.contains("kimi")
        || lower.contains("minimax")
        || lower.contains("longcat")
        || lower.contains("muse-spark")
        || lower.contains("space-bunny")
        || lower.contains("hy3")
        || lower.contains("hy4")
        || lower.contains("llama-3.1")
        || lower.contains("llama-4")
        || lower.contains("mistral")
    {
        Some(crate::db::ToolCapability::Native)
    } else if lower.contains("gemma3")
        || lower.contains("gemma-3")
        || lower.contains("deepseek-v3")
        || lower.contains("deepseek-r1")
    {
        Some(crate::db::ToolCapability::PromptFallback)
    } else {
        None
    }
}

// ── ReAct loop ────────────────────────────────────────────────────────────────

pub async fn run_react_loop(
    api_key: String,
    mut history: Vec<serde_json::Value>,
    injected_config: HashMap<String, HashMap<String, String>>,
    skills: std::sync::Arc<crate::skills::SkillManager>,
    tx: mpsc::Sender<AgentEvent>,
    user_prompt: String,
    skills_type: Option<String>,
    db: std::sync::Arc<crate::db::Db>,
    payment_vault: Option<std::sync::Arc<crate::payments::direct::PaymentVault>>,
    x402_vault: Option<std::sync::Arc<crate::payments::x402_vault::X402PaymentVault>>,
    agent_did: String,
    task_id: String,
    pending_action: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let mut skill_fire_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    let mut step = 0;
    let mut force_fallback_this_turn = false;

    // ── Resume: a plain ask just needs its reply folded back into history ──────
    // No fingerprinting/holds involved — this isn't a decision-grade
    // confirmation, just "the user answered a clarifying question." Falls
    // through into the normal loop below instead of returning early.
    let mut pending_action = match pending_action {
        Some(ref pending) if pending.get("kind").and_then(|v| v.as_str()) == Some("ask") => {
            let _ = db.clear_pending_action(&task_id);
            history.push(json!({ "role": "user", "content": user_prompt.clone() }));
            None
        }
        other => other,
    };

    // ── Resume: a pending skill-grant decision (forge install) ───────────────
    // Same park/resume shape as payment holds: the grant Ask parked history +
    // pending JSON via `save_awaiting_confirmation`; this reply resolves it
    // deterministically (never re-asked to the model).
    if let Some(ref pending) = pending_action
        && crate::agent::forge::is_skill_grant_pending(pending)
    {
        let grant_skill =
            pending.get("skill").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        match confirmation_decision(&user_prompt) {
            ConfirmationDecision::Confirmed => {
                match crate::agent::forge::approve_skill_grant(&db, &skills, &task_id, pending)
                    .await
                {
                    crate::agent::forge::GrantOutcome::Installed { observation } => {
                        let _ = tx
                            .send(AgentEvent::Observation {
                                content: observation.clone(),
                                span_id: Some(new_span_id()),
                                parent_span_id: None,
                                depth: Some(0),
                            })
                            .await;
                        history.push(json!({
                            "role": "user",
                            "content": format!("User granted the install. Result: {}", observation)
                        }));
                        pending_action = None;
                    }
                    crate::agent::forge::GrantOutcome::FingerprintChanged {
                        question,
                        refreshed_pending,
                    } => {
                        let _ = tx
                            .send(AgentEvent::Ask {
                                content: question.clone(),
                                task_id: task_id.clone(),
                                kind: Some(AskKind::SkillGrant),
                            })
                            .await;
                        history.push(json!({
                            "role": "assistant",
                            "content": format!(
                                "Skill grant refreshed because staging changed since the grant was issued: {}",
                                grant_skill
                            )
                        }));
                        let history_json = serde_json::to_string(&history).unwrap_or_default();
                        let pending_json =
                            serde_json::to_string(&refreshed_pending).unwrap_or_default();
                        let _ =
                            db.save_awaiting_confirmation(&task_id, &history_json, &pending_json);
                        let _ = tx.send(AgentEvent::Done).await;
                        return Ok(());
                    }
                    crate::agent::forge::GrantOutcome::Refused { message } => {
                        // Air-gap fail-closed: the grant stays parked so the
                        // user can enable DLT and approve again.
                        let _ = tx.send(AgentEvent::Error { content: message }).await;
                        let _ = tx.send(AgentEvent::Done).await;
                        return Ok(());
                    }
                    crate::agent::forge::GrantOutcome::Failed { message } => {
                        let _ = tx.send(AgentEvent::Error { content: message }).await;
                        let _ = tx.send(AgentEvent::Done).await;
                        return Ok(());
                    }
                }
            }
            ConfirmationDecision::Denied => {
                let message = crate::agent::forge::discard_staged_skill(&grant_skill);
                let _ = db.clear_pending_action(&task_id);
                let _ = tx
                    .send(AgentEvent::Final {
                        content: message,
                        span_id: Some(new_span_id()),
                        parent_span_id: None,
                        depth: Some(0),
                    })
                    .await;
                let _ = tx.send(AgentEvent::Done).await;
                return Ok(());
            }
            ConfirmationDecision::ContinueConversation => {
                history.push(json!({
                    "role": "user",
                    "content": crate::agent::forge::skill_grant_resume_context(pending, &user_prompt)
                }));
                // Skip the payment block below; the DB pending stays parked so
                // a later yes/no still resolves the same grant.
                pending_action = None;
            }
        }
    }

    // ── Resume: a pending payment confirmation takes priority over the LLM ─────
    // Interpreted deterministically (not re-asked to the model) so a
    // confirmation reply can't be reinterpreted into a different action right
    // at the point money would move.
    if let Some(pending) = pending_action {
        let skill = pending.get("skill").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let args = pending.get("args").cloned().unwrap_or_else(|| json!({}));
        let stored_fingerprint =
            pending.get("fingerprint").and_then(|v| v.as_str()).unwrap_or_default();

        if !skill_requires_confirmation(&skill) {
            let _ = db.clear_pending_action(&task_id);
            let _ = tx
                .send(AgentEvent::Error {
                    content: "Stored payment confirmation is invalid and was not executed.".into(),
                })
                .await;
            let _ = tx.send(AgentEvent::Done).await;
            return Ok(());
        }

        match confirmation_decision(&user_prompt) {
            ConfirmationDecision::Confirmed => {
                match approve_hold(
                    &db,
                    &skills,
                    payment_vault.clone(),
                    x402_vault.clone(),
                    &injected_config,
                    &agent_did,
                    &task_id,
                    &skill,
                    &args,
                    stored_fingerprint,
                    Some(&tx),
                )
                .await
                {
                    ApproveHoldOutcome::Invalid { message } => {
                        let _ = tx.send(AgentEvent::Error { content: message }).await;
                        let _ = tx.send(AgentEvent::Done).await;
                        return Ok(());
                    }
                    ApproveHoldOutcome::FingerprintChanged { question, refreshed_pending } => {
                        let _ = tx
                            .send(AgentEvent::Ask {
                                content: question,
                                task_id: task_id.clone(),
                                kind: Some(AskKind::Payment),
                            })
                            .await;

                        history.push(json!({
                            "role": "assistant",
                            "content": format!(
                                "Payment confirmation refreshed because the pending transaction fingerprint changed: {}",
                                payment_action_summary(&skill, &args)
                            )
                        }));

                        let history_json = serde_json::to_string(&history).unwrap_or_default();
                        let pending_json =
                            serde_json::to_string(&refreshed_pending).unwrap_or_default();
                        let _ =
                            db.save_awaiting_confirmation(&task_id, &history_json, &pending_json);

                        let _ = tx.send(AgentEvent::Done).await;
                        return Ok(());
                    }
                    ApproveHoldOutcome::Executed { success, observation, .. } => {
                        let _ = tx
                            .send(AgentEvent::Observation {
                                content: observation.clone(),
                                span_id: Some(new_span_id()),
                                parent_span_id: None,
                                depth: Some(0),
                            })
                            .await;

                        if !success {
                            let _ = tx.send(AgentEvent::Error { content: observation }).await;
                            let _ = tx.send(AgentEvent::Done).await;
                            return Ok(());
                        }

                        history.push(json!({
                            "role": "user",
                            "content": format!("User confirmed. Result of {}: {}", skill, observation)
                        }));
                        // Falls through into the normal loop below so the LLM can
                        // synthesize a final user-facing response from the observation.
                    }
                }
            }
            ConfirmationDecision::Denied => {
                let message = release_hold(&db, &agent_did, &task_id, &skill, &args);
                let _ = tx
                    .send(AgentEvent::Final {
                        content: message,
                        span_id: Some(new_span_id()),
                        parent_span_id: None,
                        depth: Some(0),
                    })
                    .await;
                let _ = tx.send(AgentEvent::Done).await;
                return Ok(());
            }
            ConfirmationDecision::ContinueConversation => {
                if let Ok((rec, amt)) = extract_payment_recipient_and_amount(&skill, &args) {
                    let pkey = compute_payment_key(&agent_did, &rec, amt);
                    let _ = db.release_spend_hold(&agent_did, &pkey);
                }
                let _ = db.clear_pending_action(&task_id);
                history.push(json!({
                    "role": "user",
                    "content": pending_payment_resume_context(&pending, &user_prompt)
                }));
            }
        }
    }

    // `step` counts turns, not tool calls: one turn may dispatch up to
    // MAX_PARALLEL_ACTIONS concurrent actions (see plan_action_batch).
    while step < MAX_REACT_STEPS {
        let settings = crate::config::resolve_llm(&db);
        let model: &str = settings.model.as_str();

        // Fetch capability every step in case it was updated
        let mut capability = {
            let db_cap =
                db.get_model_capability(model).unwrap_or(crate::db::ToolCapability::Unverified);
            if db_cap == crate::db::ToolCapability::Unverified {
                if let Some(static_cap) = static_capability(model) {
                    let _ = db.set_model_capability(model, static_cap.clone());
                    static_cap
                } else {
                    db_cap
                }
            } else {
                db_cap
            }
        };

        if force_fallback_this_turn {
            capability = crate::db::ToolCapability::PromptFallback;
            force_fallback_this_turn = false;
        }

        let is_native = capability == crate::db::ToolCapability::Native
            || capability == crate::db::ToolCapability::Unverified;
        let user_profile = db.get_user_profile(&agent_did).ok().flatten();
        // Live read per step (not the startup-cached RuntimeConfig clone):
        // flipping the flag mid-task hides DLT skills from the next prompt.
        let dlt_enabled = crate::config::dlt_enabled_live(&db);
        let sys_prompt = system_prompt_with_dlt(
            &user_prompt,
            skills_type.clone(),
            is_native,
            user_profile.as_ref(),
            dlt_enabled,
        );
        let tools = if is_native {
            Some(build_native_tools_with_dlt(&user_prompt, &skills_type, dlt_enabled))
        } else {
            None
        };

        tracing::info!("System Prompt sent to LLM: \n{}", sys_prompt);

        // Stream the LLM response.
        let stream_result = call_llm_streaming(
            &settings,
            &api_key,
            &task_id,
            &sys_prompt,
            &history,
            &tx,
            tools,
            is_native,
        )
        .await;

        let (raw, tool_calls) = match stream_result {
            Ok(r) => r,
            Err(e) => {
                let err_str = e.to_string();
                if capability == crate::db::ToolCapability::Unverified
                    && err_str.starts_with("TOOL_REJECTION_ERROR")
                {
                    tracing::warn!(
                        "Model explicitly rejected tools. Saving PromptFallback and retrying."
                    );
                    if let Ok(db) = crate::db::Db::new() {
                        let _ = db
                            .set_model_capability(model, crate::db::ToolCapability::PromptFallback);
                    }
                    continue; // Retry this step
                }

                let clean_err = err_str.replace("TOOL_REJECTION_ERROR ", "");
                let _ = tx
                    .send(AgentEvent::Error { content: format!("LLM error: {}", clean_err) })
                    .await;
                let _ = tx.send(AgentEvent::Done).await;
                return Err(e);
            }
        };

        tracing::info!("LLM Raw Response: '{}', Tool Calls: {:?}", raw, tool_calls);

        if capability == crate::db::ToolCapability::Unverified && !tool_calls.is_empty() {
            tracing::info!("Tool calls observed. Saving Native capability.");
            if let Ok(db) = crate::db::Db::new() {
                let _ = db.set_model_capability(model, crate::db::ToolCapability::Native);
            }
        }

        let mut parsed = Vec::new();
        if !is_native {
            parsed = parse_agent_responses(&raw);
            if parsed.is_empty() {
                let _ = tx.send(AgentEvent::Done).await;
                return Ok(());
            }
        } else {
            if !tool_calls.is_empty() {
                for tc in tool_calls.iter() {
                    if let Some(func) = tc.get("function")
                        && let (Some(name), Some(args_str)) =
                            (func.get("name"), func.get("arguments"))
                    {
                        let name = name.as_str().unwrap_or_default().to_string();
                        let args_text = args_str.as_str().unwrap_or_default();
                        let args: serde_json::Value =
                            serde_json::from_str(args_text).unwrap_or_else(|_| json!({}));

                        if name == crate::agent::prompt::ASK_TOOL_NAME {
                            let question = args
                                .get("question")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(no question provided)")
                                .to_string();
                            parsed.push(AgentResponseKind::Ask(question));
                        } else {
                            parsed.push(AgentResponseKind::Action { skill: name, args });
                        }
                    }
                }
            } else if !raw.is_empty() {
                if capability == crate::db::ToolCapability::Unverified {
                    tracing::warn!(
                        "Ambiguous plain text from Unverified model. Retrying turn via PromptFallback."
                    );
                    force_fallback_this_turn = true;
                    continue; // Retry this step
                } else {
                    // Already Native, so plain text is just Final, unless it contains a question
                    if is_question(&raw) {
                        parsed.push(AgentResponseKind::Ask(raw.clone()));
                    } else {
                        parsed.push(AgentResponseKind::Final(raw.clone()));
                    }
                }
            } else {
                let _ = tx.send(AgentEvent::Done).await;
                return Ok(());
            }
        }

        let mut should_continue = true;
        let mut executed_tools = false;

        // ── Parallel multi-action planning (deterministic, no model calls) ──
        // The first run of consecutive Actions in this turn is planned here:
        // pure non-payment runs go to `execute_parallel_batch`, runs with a
        // payment action execute only the first payment action sequentially
        // (rest deferred). See `plan_action_batch`.
        let batch_plan = plan_action_batch(&parsed, &db, &tool_calls);
        let mut parallel_at: Option<(usize, usize, Vec<BatchItem>, Vec<String>)> = None;
        let mut payment_skip: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        let mut payment_at: Option<usize> = None;
        let mut payment_note: Option<String> = None;
        match batch_plan {
            Some(BatchPlan::Parallel { start, run_end, items, extra_desc }) => {
                parallel_at = Some((start, run_end, items, extra_desc));
            }
            Some(BatchPlan::PaymentFirst { payment_pos, skip, note }) => {
                payment_at = Some(payment_pos);
                payment_skip = skip.into_iter().collect();
                payment_note = Some(note);
            }
            None => {}
        }

        for (pos, kind) in parsed.into_iter().enumerate() {
            if payment_skip.contains(&pos) {
                continue;
            }
            if let Some((start, run_end, _, _)) = parallel_at.as_ref()
                && pos > *start
                && pos < *run_end
            {
                // Handled by the batch executor (executed) or covered by the
                // deferral note (extras over the cap): skip silently.
                continue;
            }
            match kind {
                AgentResponseKind::Chat(content) => {
                    let _ = tx.send(AgentEvent::Chat { content }).await;
                    should_continue = false;
                }
                AgentResponseKind::Thought(thought) => {
                    let _ = tx
                        .send(AgentEvent::Thought {
                            content: thought,
                            span_id: Some(new_span_id()),
                            parent_span_id: None,
                            depth: Some(0),
                        })
                        .await;
                }
                AgentResponseKind::Action { skill, args } => {
                    if let Some((start, _, _, _)) = parallel_at.as_ref()
                        && pos == *start
                    {
                        let (_, _, items, extra_desc) =
                            parallel_at.take().expect("parallel batch planned");
                        let _ = (skill, args);
                        executed_tools = true;
                        should_continue = execute_parallel_batch(
                            items,
                            extra_desc,
                            &db,
                            &skills,
                            &payment_vault,
                            &x402_vault,
                            &injected_config,
                            &agent_did,
                            &task_id,
                            &tx,
                            &mut history,
                            &mut skill_fire_counts,
                            &raw,
                            is_native,
                        )
                        .await;
                        if !should_continue {
                            break;
                        }
                        continue;
                    }
                    if Some(pos) == payment_at
                        && let Some(note) = payment_note.take()
                    {
                        let _ = tx
                            .send(AgentEvent::Observation {
                                content: note.clone(),
                                span_id: Some(new_span_id()),
                                parent_span_id: None,
                                depth: Some(0),
                            })
                            .await;
                        history.push(json!({ "role": "user", "content": note }));
                    }
                    executed_tools = true;
                    // Air-gap proposal guard: DLT skills fail closed BEFORE
                    // any proposal validation, hold reservation, or HCS audit
                    // write below — all of those are unreachable once this
                    // returns an error, which is also what keeps HCS egress
                    // silent in air-gap mode (no audit.rs change needed).
                    // Covers x402_pay too, which never enters the
                    // skill_requires_confirmation path.
                    if let Some(message) = dlt_blocked_error(&db, &skill) {
                        let _ = tx.send(AgentEvent::Error { content: message }).await;
                        should_continue = false;
                        break;
                    }
                    let react_meta = load_react_meta(&skill);
                    if let Some(max) = react_meta.max_steps {
                        let count = skill_fire_counts.entry(skill.clone()).or_insert(0);
                        if *count >= max {
                            let _ = tx
                                .send(AgentEvent::Error {
                                    content: format!(
                                        "Skill '{}' exceeded its max_steps limit of {}",
                                        skill, max
                                    ),
                                })
                                .await;
                            should_continue = false;
                            break;
                        }
                        *count += 1;
                    }
                    if skill_requires_confirmation(&skill) {
                        if let Some(error) = payment_proposal_error(&skill, &args) {
                            let _ = tx.send(AgentEvent::Error { content: error }).await;
                            should_continue = false;
                            break;
                        }

                        let runtime_cfg = crate::config::RuntimeConfig::load(&db);
                        let governance = &runtime_cfg.governance;
                        let audit_client = payment_vault
                            .as_ref()
                            .map(|v| v.client())
                            .or_else(|| x402_vault.as_ref().map(|v| v.client()));
                        let topic_id = governance.audit_topic_id.clone();
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;

                        let (recipient, amount_hbar) =
                            match extract_payment_recipient_and_amount(&skill, &args) {
                                Ok(res) => res,
                                Err(err_msg) => {
                                    let _ = tx.send(AgentEvent::Error { content: err_msg }).await;
                                    should_continue = false;
                                    break;
                                }
                            };

                        // Check 1a: Allowlist (aria.allowlist)
                        let is_allowed =
                            db.is_account_allowlisted(&agent_did, &recipient).unwrap_or(false);
                        crate::payments::audit::write_payment_decision(
                            audit_client.clone(),
                            topic_id.clone(),
                            crate::payments::audit::AriaRecord {
                                v: 1,
                                agent: agent_did.clone(),
                                ts: now_ms,
                                policy: Some("aria.allowlist".to_string()),
                                method: Some(skill.clone()),
                                amount: Some(amount_hbar),
                                currency: Some("HBAR".to_string()),
                                counterparty: Some(recipient.clone()),
                                allowed: Some(is_allowed),
                                reason: Some(if is_allowed {
                                    "allowlisted".to_string()
                                } else {
                                    format!("not_allowlisted:{}", recipient)
                                }),
                                request_id: None,
                            },
                        );

                        if !is_allowed {
                            let err_msg = format!(
                                "Payment blocked by policy (aria.allowlist): account '{}' is not on the allowlist.",
                                recipient
                            );
                            let _ = tx.send(AgentEvent::Error { content: err_msg }).await;
                            should_continue = false;
                            break;
                        }

                        // Check 1b: Spend Limit (aria.spend-limit)
                        if let Some(per_task) = governance.per_task_cap {
                            if amount_hbar > per_task {
                                crate::payments::audit::write_payment_decision(
                                    audit_client.clone(),
                                    topic_id.clone(),
                                    crate::payments::audit::AriaRecord {
                                        v: 1,
                                        agent: agent_did.clone(),
                                        ts: now_ms,
                                        policy: Some("aria.spend-limit".to_string()),
                                        method: Some(skill.clone()),
                                        amount: Some(amount_hbar),
                                        currency: Some("HBAR".to_string()),
                                        counterparty: Some(recipient.clone()),
                                        allowed: Some(false),
                                        reason: Some("per_task_exceeded".to_string()),
                                        request_id: None,
                                    },
                                );

                                let err_msg = format!(
                                    "Payment blocked by policy (aria.spend-limit): amount {} HBAR exceeds per-task cap of {} HBAR.",
                                    amount_hbar, per_task
                                );
                                let _ = tx.send(AgentEvent::Error { content: err_msg }).await;
                                should_continue = false;
                                break;
                            }
                        }

                        let pkey = compute_payment_key(&agent_did, &recipient, amount_hbar);
                        let reserved = db
                            .try_reserve_spend(
                                &agent_did,
                                &pkey,
                                amount_hbar,
                                governance.per_day_cap,
                            )
                            .unwrap_or(false);

                        crate::payments::audit::write_payment_decision(
                            audit_client.clone(),
                            topic_id.clone(),
                            crate::payments::audit::AriaRecord {
                                v: 1,
                                agent: agent_did.clone(),
                                ts: now_ms,
                                policy: Some("aria.spend-limit".to_string()),
                                method: Some(skill.clone()),
                                amount: Some(amount_hbar),
                                currency: Some("HBAR".to_string()),
                                counterparty: Some(recipient.clone()),
                                allowed: Some(reserved),
                                reason: Some(if reserved {
                                    "within_budget".to_string()
                                } else {
                                    "per_day_exceeded".to_string()
                                }),
                                request_id: None,
                            },
                        );

                        if !reserved {
                            let err_msg = format!(
                                "Payment blocked by policy (aria.spend-limit): payment of {} HBAR exceeds rolling 24-hour daily budget cap.",
                                amount_hbar
                            );
                            let _ = tx.send(AgentEvent::Error { content: err_msg }).await;
                            should_continue = false;
                            break;
                        }

                        // Check 1c: Approval Tier (aria.approval-tier)
                        let auto_approved = match governance.auto_under {
                            Some(threshold) if amount_hbar <= threshold => true,
                            _ => false,
                        };

                        if auto_approved {
                            crate::payments::audit::write_payment_decision(
                                audit_client.clone(),
                                topic_id.clone(),
                                crate::payments::audit::AriaRecord {
                                    v: 1,
                                    agent: agent_did.clone(),
                                    ts: now_ms,
                                    policy: Some("aria.approval-tier".to_string()),
                                    method: Some(skill.clone()),
                                    amount: Some(amount_hbar),
                                    currency: Some("HBAR".to_string()),
                                    counterparty: Some(recipient.clone()),
                                    allowed: Some(true),
                                    reason: Some("auto_approved".to_string()),
                                    request_id: None,
                                },
                            );
                            // Hold is committed/released AFTER run_skill_raw below, not here.
                            // (pkey is captured in the outer scope for use post-execution.)
                        } else {
                            crate::payments::audit::write_payment_decision(
                                audit_client.clone(),
                                topic_id.clone(),
                                crate::payments::audit::AriaRecord {
                                    v: 1,
                                    agent: agent_did.clone(),
                                    ts: now_ms,
                                    policy: Some("aria.approval-tier".to_string()),
                                    method: Some(skill.clone()),
                                    amount: Some(amount_hbar),
                                    currency: Some("HBAR".to_string()),
                                    counterparty: Some(recipient.clone()),
                                    allowed: Some(false),
                                    reason: Some("approval_required".to_string()),
                                    request_id: None,
                                },
                            );

                            let question = payment_confirmation_message(&skill, &args);
                            let _ = tx
                                .send(AgentEvent::Ask {
                                    content: question,
                                    task_id: task_id.clone(),
                                    kind: Some(AskKind::Payment),
                                })
                                .await;

                            history.push(json!({
                                "role": "assistant",
                                "content": format!(
                                    "Proposed payment action awaiting human confirmation: {}",
                                    payment_action_summary(&skill, &args)
                                )
                            }));

                            let history_json = serde_json::to_string(&history).unwrap_or_default();
                            let pending_json = serde_json::to_string(&pending_payment_action(
                                &skill,
                                args.clone(),
                                &injected_config,
                            ))
                            .unwrap_or_default();
                            let _ = db.save_awaiting_confirmation(
                                &task_id,
                                &history_json,
                                &pending_json,
                            );

                            should_continue = false;
                            break;
                        }
                    }
                    let action_span_id = new_span_id();
                    let _ = tx
                        .send(AgentEvent::Action {
                            skill: skill.clone(),
                            args: args.clone(),
                            span_id: Some(action_span_id.clone()),
                            parent_span_id: None,
                            depth: Some(0),
                        })
                        .await;

                    let mut enriched = args.clone();
                    if let Some(obj) = enriched.as_object_mut()
                        && let Some(skill_config) = injected_config.get(&skill)
                    {
                        for (k, v) in skill_config {
                            obj.insert(k.clone(), json!(v));
                        }
                    }

                    // ── deterministic: no model calls past this point ──
                    // Same executor boundary as in `approve_hold`: `run_skill_raw`
                    // + hold commit/release below are deterministic dispatch only.
                    // Failures return as structured error observations for the
                    // loop to reason about — never an LLM retry from in here.
                    let (observation, tx_id, is_error): (String, Option<String>, bool) =
                        match skills
                            .run_skill_raw(
                                &skill,
                                &enriched,
                                Some(db.clone()),
                                payment_vault.clone(),
                                x402_vault.clone(),
                                agent_did.clone(),
                                Some(task_id.clone()),
                            )
                            .await
                        {
                            Ok(val) => {
                                let tx_id = val
                                    .get("transaction_id")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                (val.to_string(), tx_id, false)
                            }
                            Err(e) => (e.to_string(), None, true),
                        };
                    let _ = tx
                        .send(AgentEvent::Observation {
                            content: observation.clone(),
                            span_id: Some(new_span_id()),
                            parent_span_id: Some(action_span_id.clone()),
                            depth: Some(1),
                        })
                        .await;

                    // For payment skills (both auto-approved and confirmed-after-policy),
                    // commit the hold on success or release it on failure.
                    if skill_requires_confirmation(&skill) {
                        if let Ok((rec, amt)) = extract_payment_recipient_and_amount(&skill, &args)
                        {
                            let pkey = compute_payment_key(&agent_did, &rec, amt);
                            if is_error {
                                let _ = db.release_spend_hold(&agent_did, &pkey);
                            } else {
                                let _ = db.commit_spend_hold(&agent_did, &pkey);
                            }
                        }
                    }

                    // Applies to any payment skill that just submitted (auto-approved
                    // hedera_pay or autonomous x402_pay) — a no-op for non-payment
                    // skills, since tx_id is only Some when the result had a
                    // "transaction_id" field.
                    if !is_error {
                        spawn_payment_settlement_watch(db.clone(), Some(tx.clone()), tx_id);
                    }

                    if react_meta.terminal && !is_error {
                        let _ = tx
                            .send(AgentEvent::Final {
                                content: observation,
                                span_id: Some(new_span_id()),
                                parent_span_id: Some(action_span_id.clone()),
                                depth: Some(1),
                            })
                            .await;
                        should_continue = false;
                        break;
                    }

                    if is_native {
                        // Native tool_calls shape history appending
                        let matched_call = tool_calls.iter().find(|tc| {
                            if let Some(f) = tc.get("function")
                                && let Some(n) = f.get("name")
                            {
                                return n.as_str().unwrap_or_default() == skill;
                            }
                            false
                        });
                        if let Some(tc) = matched_call {
                            let tc_id = tc.get("id").and_then(|id| id.as_str()).unwrap_or_default();
                            history
                                .push(json!({ "role": "assistant", "tool_calls": [tc.clone()] }));
                            let content_val = if is_error {
                                format!("Error: {}", observation)
                            } else {
                                observation
                            };
                            history.push(json!({ "role": "tool", "tool_call_id": tc_id, "content": content_val }));
                        }
                    } else {
                        // Fallback logic
                        history.push(json!({ "role": "assistant", "content": raw.clone() }));
                        let label = if is_error { "Error from" } else { "Observation from" };
                        history.push(json!({
                            "role": "user",
                            "content": format!("{} {}: {}. If this is an error, consider retrying with corrected args, trying a different skill, or telling the user it failed.", label, skill, observation)
                        }));
                    }
                }
                AgentResponseKind::Ask(question) => {
                    let _ = tx
                        .send(AgentEvent::Ask {
                            content: question.clone(),
                            task_id: task_id.clone(),
                            kind: None,
                        })
                        .await;

                    // Persist so the next message on this task_id resumes the
                    // conversation instead of starting a fresh task with no
                    // memory of what was asked. Unlike a payment ask, there's
                    // no skill/args to hold open — just a marker so the top of
                    // this function knows to fold the reply back into history
                    // and continue, rather than trying to interpret it as a
                    // payment confirmation.
                    history.push(json!({
                        "role": "assistant",
                        "content": format!("Asked the user: {}", question)
                    }));
                    let history_json = serde_json::to_string(&history).unwrap_or_default();
                    let _ =
                        db.save_awaiting_confirmation(&task_id, &history_json, r#"{"kind":"ask"}"#);

                    should_continue = false;
                }
                AgentResponseKind::Final(answer) => {
                    let _ = tx
                        .send(AgentEvent::Final {
                            content: answer,
                            span_id: Some(new_span_id()),
                            parent_span_id: None,
                            depth: Some(0),
                        })
                        .await;
                    should_continue = false;
                }
            }
        }

        if !should_continue {
            let _ = tx.send(AgentEvent::Done).await;
            return Ok(());
        }

        step += 1;
    }

    let _ = tx
        .send(AgentEvent::Error { content: "Max steps reached without a final answer.".into() })
        .await;
    let _ = tx.send(AgentEvent::Done).await;
    Ok(())
}

// ── Parallel multi-action dispatch ────────────────────────────────────────────
// Tier 1 of the hierarchy plan: the model may emit up to MAX_PARALLEL_ACTIONS
// independent `Action{skill,args}` in a single turn ("read 5 docs" costs one
// turn, not five). MAX_REACT_STEPS still counts turns, not calls.
//
// Safety rules (all decided deterministically here — no model calls):
// - A batch is the first maximal run of *consecutive* Actions in the turn;
//   anything else (Thought, Ask, Final, Chat) breaks the run and flows
//   through the sequential loop. Later runs stay sequential.
// - DLT-blocked or payment-confirmation actions never run concurrently: a
//   run containing a DLT-blocked action stays fully sequential (today's path
//   fails closed per action); a run containing a payment action executes only
//   the first payment action via today's sequential path while the rest are
//   deferred, never executed.
// - Holds are never reserved concurrently: the concurrent path only ever
//   contains non-payment skills, which reserve no holds.

/// One action inside a planned parallel batch, with its native tool call
/// (if any) pre-matched so duplicate skill names still pair with distinct
/// call ids in history.
struct BatchItem {
    skill: String,
    args: serde_json::Value,
    tool_call: Option<serde_json::Value>,
    tool_call_id: String,
}

enum BatchPlan {
    Parallel {
        start: usize,
        run_end: usize,
        items: Vec<BatchItem>,
        extra_desc: Vec<String>,
    },
    PaymentFirst {
        payment_pos: usize,
        skip: Vec<usize>,
        note: String,
    },
}

fn action_summary(skill: &str, args: &serde_json::Value) -> String {
    format!("{} {}", skill, args)
}

fn plan_action_batch(
    parsed: &[AgentResponseKind],
    db: &crate::db::Db,
    tool_calls: &[serde_json::Value],
) -> Option<BatchPlan> {
    // Locate the first maximal run of consecutive Actions.
    let mut run_start: Option<usize> = None;
    let mut run_end = 0usize;
    for (i, kind) in parsed.iter().enumerate() {
        if matches!(kind, AgentResponseKind::Action { .. }) {
            if run_start.is_none() {
                run_start = Some(i);
            }
            run_end = i + 1;
        } else if run_start.is_some() {
            break;
        }
    }
    let start = run_start?;
    if run_end - start < 2 {
        return None;
    }

    let mut run: Vec<(String, serde_json::Value)> = Vec::new();
    for kind in &parsed[start..run_end] {
        if let AgentResponseKind::Action { skill, args } = kind {
            run.push((skill.clone(), args.clone()));
        }
    }

    // Air-gap fail-closed: any DLT-blocked action keeps the whole turn on
    // today's sequential path, which surfaces the block per action.
    if run.iter().any(|(skill, _)| dlt_blocked_error(db, skill).is_some()) {
        return None;
    }

    // Payment governance: never concurrent. Only the first payment action
    // runs (via today's proposal/Ask/park path); the rest are deferred.
    if let Some(rel) = run.iter().position(|(skill, _)| skill_requires_confirmation(skill)) {
        let payment_pos = start + rel;
        let mut skip = Vec::new();
        let mut deferred = Vec::new();
        for (k, (skill, args)) in run.iter().enumerate() {
            if k != rel {
                skip.push(start + k);
                deferred.push(action_summary(skill, args));
            }
        }
        let note = format!(
            "Deferred {} parallel action(s) this turn: {}. The payment action '{}' requires sequential confirmation, so the rest did not run — re-emit any still-needed actions after it resolves.",
            deferred.len(),
            deferred.join("; "),
            run[rel].0,
        );
        return Some(BatchPlan::PaymentFirst { payment_pos, skip, note });
    }

    // Pure non-payment run → concurrent batch, capped at MAX_PARALLEL_ACTIONS.
    let take = run.len().min(MAX_PARALLEL_ACTIONS);
    // Match native tool calls to items in order (first-unused same-name win).
    let mut used = vec![false; tool_calls.len()];
    let mut items = Vec::with_capacity(take);
    for (skill, args) in run.iter().take(take) {
        let mut tool_call = None;
        let mut tool_call_id = String::new();
        for (j, call) in tool_calls.iter().enumerate() {
            if used[j] {
                continue;
            }
            let name =
                call.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or_default();
            if name == skill {
                used[j] = true;
                tool_call_id =
                    call.get("id").and_then(|id| id.as_str()).unwrap_or_default().to_string();
                tool_call = Some(call.clone());
                break;
            }
        }
        items.push(BatchItem {
            skill: skill.clone(),
            args: args.clone(),
            tool_call,
            tool_call_id,
        });
    }
    let extra_desc =
        run.iter().skip(take).map(|(skill, args)| action_summary(skill, args)).collect();
    Some(BatchPlan::Parallel { start, run_end, items, extra_desc })
}

/// Executes a planned parallel batch of non-payment actions concurrently and
/// appends one labeled Observation per action — in action order (deterministic
/// history), never completion order. Returns `should_continue` for the turn
/// loop. At most one terminal skill fires per batch: the first successful
/// terminal result (in action order) becomes Final and ends the turn.
///
/// Concurrency safety: `SkillManager::run_skill_raw` only borrows `&self`
/// (module cache behind `RwLock`; `wasmtime::Engine` is `Send + Sync`) and
/// every invocation builds a fresh WASM `Store` (see `wasm_runtime.rs`), so
/// sharing one `&SkillManager` across `join_all` futures is sound — no mutex
/// needed. Each future owns its enriched args plus cloned vault/db handles.
/// Payment skills can never reach here (the planner routes them to the
/// sequential path), so no spend holds are reserved or committed concurrently
/// and no hold commit/release logic is duplicated here.
///
/// ── deterministic: no model calls past this point ── (same executor
/// boundary as the sequential path and `approve_hold`).
#[allow(clippy::too_many_arguments)]
async fn execute_parallel_batch(
    items: Vec<BatchItem>,
    extra_desc: Vec<String>,
    db: &std::sync::Arc<crate::db::Db>,
    skills: &std::sync::Arc<crate::skills::SkillManager>,
    payment_vault: &Option<std::sync::Arc<crate::payments::direct::PaymentVault>>,
    x402_vault: &Option<std::sync::Arc<crate::payments::x402_vault::X402PaymentVault>>,
    injected_config: &HashMap<String, HashMap<String, String>>,
    agent_did: &str,
    task_id: &str,
    tx: &mpsc::Sender<AgentEvent>,
    history: &mut Vec<serde_json::Value>,
    skill_fire_counts: &mut std::collections::HashMap<String, usize>,
    raw: &str,
    is_native: bool,
) -> bool {
    let n = items.len();
    // Siblings share one parent span id so the GUI renders them as one
    // expandable group: turn-span (implicit) → Action depth 1 →
    // Observation/Final depth 2.
    let turn_span = new_span_id();

    enum Slot {
        Skipped { skill: String, max: usize },
        Ready {
            skill: String,
            args: serde_json::Value,
            enriched: serde_json::Value,
            terminal: bool,
            tool_call: Option<serde_json::Value>,
            tool_call_id: String,
            action_span: String,
        },
    }

    // Deterministic pre-pass: per-skill max_steps caps apply per action — a
    // skill at its cap is skipped with an error observation, others proceed.
    let mut slots: Vec<Slot> = Vec::with_capacity(n);
    for item in items {
        let react_meta = load_react_meta(&item.skill);
        if let Some(max) = react_meta.max_steps {
            let count = skill_fire_counts.entry(item.skill.clone()).or_insert(0);
            if *count >= max {
                slots.push(Slot::Skipped { skill: item.skill, max });
                continue;
            }
            *count += 1;
        }
        let mut enriched = item.args.clone();
        if let Some(obj) = enriched.as_object_mut()
            && let Some(skill_config) = injected_config.get(&item.skill)
        {
            for (k, v) in skill_config {
                obj.insert(k.clone(), json!(v));
            }
        }
        slots.push(Slot::Ready {
            skill: item.skill,
            args: item.args,
            enriched,
            terminal: react_meta.terminal,
            tool_call: item.tool_call,
            tool_call_id: item.tool_call_id,
            action_span: new_span_id(),
        });
    }

    // Emit Action events first, in action order.
    for slot in &slots {
        if let Slot::Ready { skill, args, action_span, .. } = slot {
            let _ = tx
                .send(AgentEvent::Action {
                    skill: skill.clone(),
                    args: args.clone(),
                    span_id: Some(action_span.clone()),
                    parent_span_id: Some(turn_span.clone()),
                    depth: Some(1),
                })
                .await;
        }
    }

    // Run the ready actions concurrently. `join_all` polls inline (no
    // `Send`/`'static` requirement) and returns results in action order.
    let futs: Vec<_> = slots
        .iter()
        .map(|slot| async move {
            match slot {
                Slot::Ready { skill, enriched, .. } => Some(
                    skills
                        .run_skill_raw(
                            skill,
                            enriched,
                            Some(db.clone()),
                            payment_vault.clone(),
                            x402_vault.clone(),
                            agent_did.to_string(),
                            Some(task_id.to_string()),
                        )
                        .await,
                ),
                Slot::Skipped { .. } => None,
            }
        })
        .collect();
    let results = futures_util::future::join_all(futs).await;

    if !is_native {
        history.push(json!({ "role": "assistant", "content": raw }));
    }
    let mut terminal_hit: Option<(String, String)> = None;
    for (i, (slot, res)) in slots.iter().zip(results).enumerate() {
        let (skill, action_span, body, is_error, tx_id, tool_call, tool_call_id, terminal) =
            match (slot, res) {
                (Slot::Skipped { skill, max }, _) => (
                    skill.clone(),
                    turn_span.clone(),
                    format!("Skill '{}' exceeded its max_steps limit of {}", skill, max),
                    true,
                    None,
                    None,
                    String::new(),
                    false,
                ),
                (
                    Slot::Ready {
                        skill,
                        action_span,
                        terminal,
                        tool_call,
                        tool_call_id,
                        ..
                    },
                    Some(Ok(val)),
                    ) => {
                    let tx_id = val
                        .get("transaction_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    (
                        skill.clone(),
                        action_span.clone(),
                        val.to_string(),
                        false,
                        tx_id,
                        tool_call.clone(),
                        tool_call_id.clone(),
                        *terminal,
                    )
                }
                (
                    Slot::Ready { skill, action_span, tool_call, tool_call_id, .. },
                    Some(Err(e)),
                ) => (
                    skill.clone(),
                    action_span.clone(),
                    e.to_string(),
                    true,
                    None,
                    tool_call.clone(),
                    tool_call_id.clone(),
                    false,
                ),
                (Slot::Ready { .. }, None) => {
                    unreachable!("ready slots always produce a result")
                }
            };
        // Labeled observation so the model can attribute results to calls.
        let labeled = format!("[{}/{} from {}]: {}", i + 1, n, skill, body);
        let (parent_span_id, depth) = match slot {
            Slot::Ready { .. } => (Some(action_span.clone()), Some(2)),
            Slot::Skipped { .. } => (Some(turn_span.clone()), Some(1)),
        };
        let _ = tx
            .send(AgentEvent::Observation {
                content: labeled.clone(),
                span_id: Some(new_span_id()),
                parent_span_id,
                depth,
            })
            .await;

        // Applies to any payment skill that just submitted — a no-op for
        // non-payment skills, since tx_id is only Some when the result had a
        // "transaction_id" field. (No hold commit/release here: confirmation
        // skills never enter the concurrent path.)
        if !is_error {
            spawn_payment_settlement_watch(db.clone(), Some(tx.clone()), tx_id);
        }

        if terminal && !is_error && terminal_hit.is_none() {
            terminal_hit = Some((body, action_span));
        }

        if is_native {
            if !is_error && tool_call.is_some() {
                let tc = tool_call.unwrap_or_else(|| json!({}));
                history.push(json!({ "role": "assistant", "tool_calls": [tc] }));
                history.push(json!({ "role": "tool", "tool_call_id": tool_call_id, "content": labeled }));
            } else {
                history.push(json!({
                    "role": "user",
                    "content": format!("{}. If this is an error, consider retrying with corrected args, trying a different skill, or telling the user it failed.", labeled)
                }));
            }
        } else {
            history.push(json!({
                "role": "user",
                "content": format!("{}. If this is an error, consider retrying with corrected args, trying a different skill, or telling the user it failed.", labeled)
            }));
        }
    }

    if !extra_desc.is_empty() {
        let note = format!(
            "Deferred {} parallel action(s) this turn (max {} per turn): {}. Re-emit any still-needed actions next turn.",
            extra_desc.len(),
            MAX_PARALLEL_ACTIONS,
            extra_desc.join("; "),
        );
        let _ = tx
            .send(AgentEvent::Observation {
                content: note.clone(),
                span_id: Some(new_span_id()),
                parent_span_id: None,
                depth: Some(0),
            })
            .await;
        history.push(json!({ "role": "user", "content": note }));
    }

    if let Some((final_body, final_parent)) = terminal_hit {
        let _ = tx
            .send(AgentEvent::Final {
                content: final_body,
                span_id: Some(new_span_id()),
                parent_span_id: Some(final_parent),
                depth: Some(2),
            })
            .await;
        return false;
    }
    true
}

fn load_react_meta(skill: &str) -> ReactConfig {
    skill_dir(skill)
        .ok()
        .and_then(|dir| load_manifest(&dir).ok())
        .map(|m| m.react)
        .unwrap_or_default()
}

fn confirmation_decision(user_prompt: &str) -> ConfirmationDecision {
    let reply = user_prompt.trim().to_lowercase();
    if matches!(reply.as_str(), "yes" | "y" | "confirm" | "ok" | "okay" | "sure")
        || reply.starts_with("go ahead")
        || reply.starts_with("do it")
    {
        return ConfirmationDecision::Confirmed;
    }

    if matches!(
        reply.as_str(),
        "no" | "n"
            | "cancel"
            | "cancelled"
            | "canceled"
            | "deny"
            | "denied"
            | "stop"
            | "don't"
            | "dont"
    ) {
        return ConfirmationDecision::Denied;
    }

    ConfirmationDecision::ContinueConversation
}

fn pending_payment_resume_context(pending: &serde_json::Value, user_prompt: &str) -> String {
    format!(
        "A payment is awaiting confirmation.\n\nPending transaction:\n{}\n\nThe user has NOT confirmed this payment.\n\nUser reply:\n{}\n\nRevise the transaction if appropriate. Do not execute any payment. If the final transaction still requires payment, present a NEW confirmation request.",
        serde_json::to_string_pretty(pending).unwrap_or_else(|_| pending.to_string()),
        serde_json::to_string(user_prompt).unwrap_or_else(|_| format!("{:?}", user_prompt)),
    )
}

fn skill_capabilities(skill: &str) -> Option<Capabilities> {
    skill_dir(skill).ok().and_then(|dir| load_manifest(&dir).ok()).map(|m| m.capabilities)
}

/// Skills that can move real money via a direct Hedera transfer (`hedera_pay`)
/// must not fire without an explicit human "yes" — the LLM proposing the call
/// is not sufficient authorization on its own.
///
/// `x402_pay` skills are intentionally excluded here: their recipient and amount
/// are unknown at proposal time (they come from the server's HTTP 402 response),
/// so the pre-flight check that works for `hedera_pay` would always fail.
/// x402 governance (allowlist, per-task cap, per-day cap, rate limit) is
/// enforced autonomously inside `host_x402_pay()` in `wasm_runtime.rs` after
/// the 402 response has been parsed — that path must not be duplicated here.
fn skill_requires_confirmation(skill: &str) -> bool {
    skill_capabilities(skill)
        .map(|capabilities| capabilities.hedera_pay)
        .unwrap_or(false)
}

/// Air-gap proposal guard. Returns `Some(error)` when `skill` needs a DLT
/// capability and `dlt_enabled` is off — the caller surfaces it as a
/// proposal error so no spend hold is ever reserved and no HCS audit write
/// fires. Returns `None` for non-DLT skills and whenever the flag is on
/// (default), leaving all current behavior untouched. Reads the flag live
/// from env/db on every call so the toggle applies without a restart.
pub(crate) fn dlt_blocked_error(db: &crate::db::Db, skill: &str) -> Option<String> {
    let capabilities = skill_capabilities(skill).unwrap_or_default();
    if !(capabilities.hedera_pay || capabilities.x402_pay) {
        return None;
    }
    if crate::config::dlt_enabled_live(db) {
        return None;
    }
    Some(format!(
        "Skill '{}' blocked: DLT disabled (air-gap mode) — re-enable with `aria dlt on`",
        skill
    ))
}

fn pending_payment_action(
    skill: &str,
    args: serde_json::Value,
    injected_config: &HashMap<String, HashMap<String, String>>,
) -> serde_json::Value {
    json!({
        "skill": skill,
        "args": args,
        "fingerprint": payment_fingerprint(skill, &args, injected_config),
    })
}

fn payment_fingerprint(
    skill: &str,
    args: &serde_json::Value,
    injected_config: &HashMap<String, HashMap<String, String>>,
) -> String {
    let mut execution_args = args.clone();
    if let Some(obj) = execution_args.as_object_mut()
        && let Some(skill_config) = injected_config.get(skill)
    {
        for (k, v) in skill_config {
            obj.insert(k.clone(), json!(v));
        }
    }

    let canonical = json!({
        "skill": skill,
        "execution_args": execution_args,
        "execution_context": payment_execution_context(skill),
        "skill_artifacts": payment_skill_artifacts(skill),
    });
    crate::crypto::sha256_hex_str(&canonical.to_string())
}

fn payment_confirmation_message(skill: &str, args: &serde_json::Value) -> String {
    let capabilities = skill_capabilities(skill).unwrap_or_default();
    let details = if capabilities.hedera_pay {
        direct_payment_details(skill, args)
    } else if capabilities.x402_pay {
        x402_payment_details(skill, args)
    } else {
        vec![("Skill".to_string(), skill.to_string()), ("Arguments".to_string(), args.to_string())]
    };

    let mut lines = vec!["Payment Confirmation".to_string(), String::new()];
    for (label, value) in details {
        lines.push(format!("{}:", label));
        lines.push(value);
        lines.push(String::new());
    }

    lines.join("\n")
}

fn direct_payment_details(skill: &str, args: &serde_json::Value) -> Vec<(String, String)> {
    let mut details = vec![
        ("Skill".to_string(), skill.to_string()),
        ("Recipient".to_string(), display_arg(args, "recipient")),
        ("Amount".to_string(), format!("{} HBAR", display_arg(args, "amount"))),
        ("Memo".to_string(), display_arg(args, "memo")),
    ];
    if let Some(network) = hedera_network() {
        details.push(("Network".to_string(), network));
    }
    if let Some(payer) = hedera_payer() {
        details.push(("Payer".to_string(), payer));
    }
    details
}

fn payment_proposal_error(skill: &str, args: &serde_json::Value) -> Option<String> {
    let capabilities = skill_capabilities(skill)?;
    if capabilities.hedera_pay {
        return direct_payment_proposal_error(skill, args);
    }
    None
}

fn direct_payment_proposal_error(skill: &str, args: &serde_json::Value) -> Option<String> {
    if args.get("recipient").and_then(|v| v.as_str()).unwrap_or("").trim().is_empty() {
        return Some(format!("Payment proposal for {} is invalid: recipient is required.", skill));
    }

    let Some(amount) = args.get("amount").and_then(json_number_as_f64) else {
        return Some(format!(
            "Payment proposal for {} is invalid: amount must be a positive number.",
            skill
        ));
    };

    if amount <= 0.0 || !amount.is_finite() {
        return Some(format!(
            "Payment proposal for {} is invalid: amount must be a positive number.",
            skill
        ));
    }

    None
}

fn json_number_as_f64(value: &serde_json::Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        return Some(n);
    }

    value.as_str()?.trim().parse::<f64>().ok()
}

fn x402_payment_details(skill: &str, args: &serde_json::Value) -> Vec<(String, String)> {
    let amount = first_present_arg(
        args,
        &["amount", "price", "max_amount", "maxAmount", "maxAmountRequired"],
    )
    .unwrap_or_else(|| "Not specified in proposed arguments".to_string());
    let payee = first_present_arg(
        args,
        &["recipient", "payee", "destination", "facilitator", "paymentAddress"],
    )
    .unwrap_or_else(|| "Not specified in proposed arguments".to_string());
    let network = first_present_arg(args, &["network", "chain", "chainId"]);
    let facilitator = first_present_arg(args, &["facilitator", "facilitator_url", "paymentUrl"])
        .or_else(x402_facilitator_url);
    let metadata = metadata_without(
        args,
        &[
            "url",
            "resource",
            "service",
            "amount",
            "price",
            "max_amount",
            "maxAmount",
            "maxAmountRequired",
            "recipient",
            "payee",
            "destination",
            "facilitator",
            "paymentAddress",
            "network",
            "chain",
            "chainId",
            "facilitator_url",
            "paymentUrl",
        ],
    );

    let mut details = vec![
        ("Skill".to_string(), skill.to_string()),
        (
            "Resource/Service".to_string(),
            first_present_arg(args, &["url", "resource", "service"])
                .unwrap_or_else(|| "Not specified in proposed arguments".to_string()),
        ),
        ("Amount".to_string(), amount),
        ("Recipient/Payee".to_string(), payee),
    ];
    if let Some(network) = network {
        details.push(("Network".to_string(), network));
    } else if let Some(network) = hedera_network() {
        details.push(("Network".to_string(), network));
    }
    if let Some(facilitator) = facilitator {
        details.push(("Facilitator".to_string(), facilitator));
    }
    if let Some(metadata) = metadata {
        details.push(("Metadata".to_string(), metadata));
    }
    details
}

fn payment_execution_context(skill: &str) -> serde_json::Value {
    let capabilities = skill_capabilities(skill).unwrap_or_default();
    let mut context = serde_json::Map::new();

    if capabilities.hedera_pay || capabilities.x402_pay {
        if let Some(network) = hedera_network() {
            context.insert("hedera_network".to_string(), json!(network));
        }
        if let Some(payer) = hedera_payer() {
            context.insert("hedera_account_id".to_string(), json!(payer));
        }
    }

    if capabilities.x402_pay
        && let Some(facilitator_url) = x402_facilitator_url()
    {
        context.insert("x402_facilitator_url".to_string(), json!(facilitator_url));
    }

    serde_json::Value::Object(context)
}

fn payment_skill_artifacts(skill: &str) -> serde_json::Value {
    let mut artifacts = serde_json::Map::new();

    if let Ok(dir) = skill_dir(skill) {
        let manifest_path = dir.join("manifest.toml");
        artifacts.insert(
            "manifest_path".to_string(),
            json!(manifest_path.to_string_lossy().to_string()),
        );
        artifacts.insert("manifest_hash".to_string(), json!(file_hash(&manifest_path)));
    }

    if let Ok(path) = wasm_path(skill) {
        artifacts.insert("wasm_path".to_string(), json!(path.to_string_lossy().to_string()));
        artifacts.insert("wasm_hash".to_string(), json!(file_hash(&path)));
    }

    serde_json::Value::Object(artifacts)
}

fn file_hash(path: &std::path::Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| crate::crypto::sha256_hex(&bytes))
}

fn hedera_network() -> Option<String> {
    Some(std::env::var("HEDERA_NETWORK").unwrap_or_else(|_| "testnet".to_string()))
}

fn hedera_payer() -> Option<String> {
    std::env::var("HEDERA_ACCOUNT_ID").ok().filter(|v| !v.trim().is_empty())
}

fn x402_facilitator_url() -> Option<String> {
    Some(
        std::env::var("X402_FACILITATOR_URL")
            .unwrap_or_else(|_| "https://x402.org/facilitator".to_string()),
    )
}

fn payment_action_summary(skill: &str, args: &serde_json::Value) -> String {
    format!("{} {}", skill, args)
}

fn display_arg(args: &serde_json::Value, key: &str) -> String {
    first_present_arg(args, &[key]).unwrap_or_else(|| "Not specified".to_string())
}

fn first_present_arg(args: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        args.get(*key).and_then(|value| {
            if value.is_null() {
                None
            } else if let Some(s) = value.as_str() {
                Some(if s.trim().is_empty() { "Not specified".to_string() } else { s.to_string() })
            } else {
                Some(value.to_string())
            }
        })
    })
}

fn metadata_without(args: &serde_json::Value, excluded: &[&str]) -> Option<String> {
    let obj = args.as_object()?;
    let mut metadata = serde_json::Map::new();
    for (key, value) in obj {
        if !excluded.iter().any(|excluded_key| excluded_key == key) {
            metadata.insert(key.clone(), value.clone());
        }
    }
    if metadata.is_empty() { None } else { Some(serde_json::Value::Object(metadata).to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_history_with_tool_roundtrip() -> Vec<serde_json::Value> {
        vec![
            json!({"role": "user", "content": "What files match foo?"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "search", "arguments": "{\"q\":\"foo\"}"},
                }],
            }),
            json!({"role": "tool", "tool_call_id": "call-1", "content": "a.txt"}),
        ]
    }

    fn sample_openai_tool() -> Vec<serde_json::Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search files",
                "parameters": {"type": "object", "properties": {}},
            },
        })]
    }

    #[test]
    fn chat_body_carries_model_messages_and_openai_tools() {
        let tools = sample_openai_tool();
        let body = build_chat_body("m", "sys", &sample_history_with_tool_roundtrip(), Some(&tools));
        assert_eq!(body["model"], json!("m"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["messages"][0], json!({"role": "system", "content": "sys"}));
        assert_eq!(body["messages"].as_array().unwrap().len(), 4);
        assert_eq!(body["tools"], json!(tools));
    }

    #[test]
    fn responses_body_flattens_tools_and_maps_tool_roundtrip() {
        let tools = sample_openai_tool();
        let body =
            build_responses_body("grok-4.7", "sys", &sample_history_with_tool_roundtrip(), Some(&tools));
        assert_eq!(body["model"], json!("grok-4.7"));
        assert_eq!(body["instructions"], json!("sys"));
        assert_eq!(
            body["tools"],
            json!([{"type": "function", "name": "search", "description": "Search files",
                    "parameters": {"type": "object", "properties": {}}}]),
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0], json!({"role": "user", "content": "What files match foo?"}));
        assert_eq!(
            input[1],
            json!({"type": "function_call", "call_id": "call-1",
                   "name": "search", "arguments": "{\"q\":\"foo\"}"}),
        );
        assert_eq!(
            input[2],
            json!({"type": "function_call_output", "call_id": "call-1", "output": "a.txt"}),
        );
    }

    #[test]
    fn messages_body_uses_input_schema_and_tool_result_blocks() {
        let tools = sample_openai_tool();
        let body =
            build_messages_body("claude-haiku-5-5", "sys", &sample_history_with_tool_roundtrip(), Some(&tools));
        assert_eq!(body["model"], json!("claude-haiku-5-5"));
        assert_eq!(body["system"], json!("sys"));
        assert_eq!(body["max_tokens"], json!(8192));
        assert_eq!(
            body["tools"],
            json!([{"name": "search", "description": "Search files",
                    "input_schema": {"type": "object", "properties": {}}}]),
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(
            messages[1]["content"],
            json!([{"type": "tool_use", "id": "call-1",
                   "name": "search", "input": {"q": "foo"}}]),
        );
        assert_eq!(
            messages[2],
            json!({"role": "user", "content": [{"type": "tool_result",
                   "tool_use_id": "call-1", "content": "a.txt"}]}),
        );
    }

    #[test]
    fn chat_frames_normalize_to_text_and_openai_tool_calls() {
        let mut state = ChatStreamState::default();
        let token = apply_chat_frame(
            &mut state,
            &json!({"choices": [{"delta": {"content": "hi"}}]}),
        );
        assert_eq!(token.as_deref(), Some("hi"));
        assert!(apply_chat_frame(&mut state, &json!({"choices": [{"delta": {}}]})).is_none());
        apply_chat_frame(
            &mut state,
            &json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call-1", "function": {"name": "search", "arguments": "{\"q\":"}},
            ]}}]}),
        );
        apply_chat_frame(
            &mut state,
            &json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "\"foo\"}"}},
            ]}}]}),
        );
        assert_eq!(state.content, "hi");
        let calls = finalize_tool_calls(state.calls.into_values().collect());
        assert_eq!(
            calls,
            vec![json!({"id": "call-1", "type": "function",
                   "function": {"name": "search", "arguments": "{\"q\":\"foo\"}"}})],
        );
    }

    #[test]
    fn responses_frames_normalize_to_text_and_openai_tool_calls() {
        let mut state = ResponsesStreamState::default();
        let token = apply_responses_frame(
            &mut state,
            &json!({"type": "response.output_text.delta", "delta": "hello"}),
        );
        assert_eq!(token.as_deref(), Some("hello"));
        apply_responses_frame(
            &mut state,
            &json!({"type": "response.output_item.added",
                   "item": {"type": "function_call", "call_id": "call-9",
                            "name": "search"}}),
        );
        apply_responses_frame(
            &mut state,
            &json!({"type": "response.function_call_arguments.delta",
                   "item_id": "call-9", "delta": "{\"q\":\"foo\"}"}),
        );
        assert_eq!(state.content, "hello");
        let mut chunks = Vec::new();
        for key in &state.order {
            if let Some(chunk) = state.calls.get(key) {
                chunks.push(chunk.clone());
            }
        }
        let calls = finalize_tool_calls(chunks);
        assert_eq!(
            calls,
            vec![json!({"id": "call-9", "type": "function",
                   "function": {"name": "search", "arguments": "{\"q\":\"foo\"}"}})],
        );
    }

    #[test]
    fn anthropic_frames_normalize_to_text_and_openai_tool_calls() {
        let mut state = AnthropicStreamState::default();
        let token = apply_anthropic_frame(
            &mut state,
            &json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "yo"}}),
        );
        assert_eq!(token.as_deref(), Some("yo"));
        apply_anthropic_frame(
            &mut state,
            &json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "id": "toolu-1", "name": "search"}}),
        );
        apply_anthropic_frame(
            &mut state,
            &json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}}),
        );
        apply_anthropic_frame(
            &mut state,
            &json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "\"foo\"}"}}),
        );
        assert_eq!(state.content, "yo");
        let calls = finalize_tool_calls(state.calls.into_values().collect());
        assert_eq!(
            calls,
            vec![json!({"id": "toolu-1", "type": "function",
                   "function": {"name": "search", "arguments": "{\"q\":\"foo\"}"}})],
        );
    }

    #[test]
    fn greeting_offer_of_help_is_not_a_question() {
        assert!(!is_question("Hello! How can I help you today?"));
        assert!(!is_question("Hi there — what can I do for you?"));
        assert!(is_question("What file should I search?"));
        assert!(is_question("Which account should I pay?"));
    }

    #[test]
    fn confirmation_decision_treats_modification_as_conversation() {
        assert_eq!(
            confirmation_decision("Can you make it 0.5 HBAR instead?"),
            ConfirmationDecision::ContinueConversation
        );
        assert_eq!(confirmation_decision("yes"), ConfirmationDecision::Confirmed);
        assert_eq!(confirmation_decision("no"), ConfirmationDecision::Denied);
    }

    #[test]
    fn ask_event_serializes_task_id_for_resume() {
        let plain_event = AgentEvent::Ask {
            content: "What should I do next?".into(),
            task_id: "task-123".into(),
            kind: None,
        };
        let payment_event = AgentEvent::Ask {
            content: "Confirm payment?".into(),
            task_id: "task-123".into(),
            kind: Some(AskKind::Payment),
        };

        let plain_json_line = serde_json::to_string(&plain_event).unwrap();
        let payment_json_line = serde_json::to_string(&payment_event).unwrap();

        assert_eq!(
            plain_json_line,
            r#"{"type":"ask","content":"What should I do next?","task_id":"task-123"}"#
        );
        assert!(!plain_json_line.contains(r#""kind""#));
        assert_eq!(
            payment_json_line,
            r#"{"type":"ask","content":"Confirm payment?","task_id":"task-123","kind":"payment"}"#
        );
    }

    #[test]
    fn direct_payment_confirmation_rejects_invalid_amounts() {
        let zero = json!({
            "recipient": "0.0.1234",
            "amount": 0,
            "memo": "Invoice #42"
        });
        let negative = json!({
            "recipient": "0.0.1234",
            "amount": -1,
            "memo": "Invoice #42"
        });
        let nonsense = json!({
            "recipient": "0.0.1234",
            "amount": "not-a-number",
            "memo": "Invoice #42"
        });

        assert!(payment_proposal_error("transfer.pay", &zero).is_some());
        assert!(payment_proposal_error("transfer.pay", &negative).is_some());
        assert!(payment_proposal_error("transfer.pay", &nonsense).is_some());
    }

    #[test]
    fn ambiguous_reply_does_not_approve_and_regenerated_payment_is_confirmed_again() {
        let args = json!({
            "recipient": "0.0.1234",
            "amount": 1.5,
            "memo": "Invoice #42"
        });
        let injected_config = HashMap::new();
        let pending = pending_payment_action("transfer.pay", args.clone(), &injected_config);

        assert_eq!(
            confirmation_decision("Can you make it 0.5 HBAR instead?"),
            ConfirmationDecision::ContinueConversation
        );

        let context = pending_payment_resume_context(&pending, "Can you make it 0.5 HBAR instead?");
        assert!(context.contains("The user has NOT confirmed this payment."));
        assert!(context.contains("Do not execute any payment."));
        assert!(context.contains("present a NEW confirmation request."));

        let revised_args = json!({
            "recipient": "0.0.1234",
            "amount": 0.5,
            "memo": "Invoice #42"
        });
        assert!(skill_requires_confirmation("transfer.pay"));
        assert!(payment_proposal_error("transfer.pay", &revised_args).is_none());

        let new_confirmation = payment_confirmation_message("transfer.pay", &revised_args);
        assert!(new_confirmation.contains("Payment Confirmation"));
        assert!(new_confirmation.contains("0.5 HBAR"));
        assert!(new_confirmation.contains("• yes — execute exactly this transaction"));
    }

    #[test]
    fn fingerprint_includes_skill_artifacts() {
        let artifacts = payment_skill_artifacts("transfer.pay");
        assert!(artifacts.get("manifest_hash").is_some());
        assert!(artifacts.get("manifest_path").is_some());
    }

    #[test]
    fn payment_key_computation_is_deterministic() {
        let key1 = compute_payment_key("did:aria:test", "0.0.1234", 5.0);
        let key2 = compute_payment_key("did:aria:test", "0.0.1234", 5.0);
        let key3 = compute_payment_key("did:aria:test", "0.0.5678", 5.0);
        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
        assert_eq!(key1.len(), 16);
    }

    #[test]
    fn auto_under_match_logic_behaves_safely() {
        let auto_under_none: Option<f64> = None;
        let auto_under_some: Option<f64> = Some(2.0);

        let is_auto_none = match auto_under_none {
            Some(threshold) if 1.0 <= threshold => true,
            _ => false,
        };
        assert!(!is_auto_none, "None auto_under must require confirmation for all amounts");

        let is_auto_below = match auto_under_some {
            Some(threshold) if 1.0 <= threshold => true,
            _ => false,
        };
        assert!(is_auto_below, "Amount below AUTO_UNDER threshold must auto-approve");

        let is_auto_equal = match auto_under_some {
            Some(threshold) if 2.0 <= threshold => true,
            _ => false,
        };
        assert!(is_auto_equal, "Amount equal to AUTO_UNDER threshold must auto-approve");

        let is_auto_above = match auto_under_some {
            Some(threshold) if 5.0 <= threshold => true,
            _ => false,
        };
        assert!(
            !is_auto_above,
            "Amount above AUTO_UNDER threshold must require human confirmation"
        );
    }
}

// ── Parsing ───────────────────────────────────────────────────────────────────

fn parse_single(line: &str) -> Option<AgentResponseKind> {
    let cleaned = line
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let v: serde_json::Value = serde_json::from_str(cleaned).ok()?;

    match v["type"].as_str()? {
        "chat" => Some(AgentResponseKind::Chat(v["content"].as_str()?.to_string())),
        "thought" => Some(AgentResponseKind::Thought(v["content"].as_str()?.to_string())),
        "ask" => Some(AgentResponseKind::Ask(v["content"].as_str()?.to_string())),
        "final" => Some(AgentResponseKind::Final(v["content"].as_str()?.to_string())),
        "action" => Some(AgentResponseKind::Action {
            skill: v["skill"].as_str()?.to_string(),
            args: v["args"].clone(),
        }),
        _ => None,
    }
}

pub(crate) fn parse_agent_responses(raw: &str) -> Vec<AgentResponseKind> {
    let mut results = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    let chars: Vec<char> = raw.chars().collect();
    let mut in_string = false;
    let mut escaped = false;

    for (i, &ch) in chars.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }

        if ch == '{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                if let Some(s) = start {
                    let slice: String = chars[s..=i].iter().collect();
                    if let Some(kind) = parse_single(&slice) {
                        results.push(kind);
                    }
                }
                start = None;
            }
        }
    }

    results
}

// ── LLM streaming call ────────────────────────────────────────────────────────
// OpenCode Go is one gateway with three wire protocols (see config::Protocol).
// Each protocol gets its own body builder + SSE frame handler below, and every
// result is normalized back to `(text, openai_tool_calls)` so the loop's
// parsing of the return contract stays untouched.

#[derive(Debug, Default, Clone)]
struct ToolCallChunk {
    id: String,
    name: String,
    arguments: String,
}

/// Split an OpenAI `{"type":"function","function":{...}}` tool def (tolerates
/// an already-flat def) into name/description/parameters.
fn openai_tool_parts(tool: &serde_json::Value) -> (String, String, serde_json::Value) {
    let inner = tool.get("function").unwrap_or(tool);
    let name = inner.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let description =
        inner.get("description").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let parameters = inner
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    (name, description, parameters)
}

fn responses_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            let (name, description, parameters) = openai_tool_parts(t);
            json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            })
        })
        .collect()
}

fn anthropic_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            let (name, description, parameters) = openai_tool_parts(t);
            json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            })
        })
        .collect()
}

/// Map loop history (OpenAI message shape) to Responses `input` items.
/// `{role:"system"}` is skipped — `instructions` carries it.
fn map_history_to_responses_input(history: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for h in history {
        match h.get("role").and_then(|r| r.as_str()).unwrap_or("") {
            "system" => {}
            "assistant" => {
                let calls =
                    h.get("tool_calls").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                if calls.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": h.get("content").cloned().unwrap_or(serde_json::Value::Null),
                    }));
                } else {
                    if let Some(text) = h.get("content").and_then(|c| c.as_str()) {
                        if !text.is_empty() {
                            out.push(json!({"role": "assistant", "content": text}));
                        }
                    }
                    for tc in &calls {
                        let func = tc.get("function");
                        out.push(json!({
                            "type": "function_call",
                            "call_id": tc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                            "name": func.and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or(""),
                            "arguments": func.and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or(""),
                        }));
                    }
                }
            }
            "tool" => {
                let output = match h.get("content").cloned().unwrap_or(serde_json::Value::Null) {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": h.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "output": output,
                }));
            }
            "user" => {
                out.push(json!({
                    "role": "user",
                    "content": h.get("content").cloned().unwrap_or(serde_json::Value::Null),
                }));
            }
            _ => {
                out.push(h.clone());
            }
        }
    }
    out
}

/// Map loop history to Anthropic `messages`. The system prompt travels in the
/// top-level `system` field, so `{role:"system"}` entries are skipped.
fn map_history_to_anthropic_messages(history: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for h in history {
        match h.get("role").and_then(|r| r.as_str()).unwrap_or("") {
            "system" => {}
            "assistant" => {
                let calls =
                    h.get("tool_calls").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                if calls.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": h.get("content").cloned().unwrap_or(serde_json::Value::Null),
                    }));
                } else {
                    let mut blocks = Vec::new();
                    if let Some(text) = h.get("content").and_then(|c| c.as_str()) {
                        if !text.is_empty() {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                    }
                    for tc in &calls {
                        let func = tc.get("function");
                        let args_str = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let input: serde_json::Value =
                            serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                            "name": func.and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or(""),
                            "input": input,
                        }));
                    }
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool" => {
                let content = match h.get("content").cloned().unwrap_or(serde_json::Value::Null) {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": h.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "content": content,
                });
                // A parallel batch emits one history entry per tool call; Anthropic
                // wants all tool_result blocks of a turn inside a single user message.
                let appended = match out.last_mut() {
                    Some(serde_json::Value::Object(last))
                        if last.get("role").and_then(|r| r.as_str()) == Some("user") =>
                    {
                        match last.get_mut("content").and_then(|c| c.as_array_mut()) {
                            Some(arr)
                                if arr
                                    .first()
                                    .and_then(|b| b.get("type"))
                                    .and_then(|t| t.as_str())
                                    == Some("tool_result") =>
                            {
                                arr.push(block.clone());
                                true
                            }
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if !appended {
                    out.push(json!({"role": "user", "content": [block]}));
                }
            }
            "user" => {
                out.push(json!({
                    "role": "user",
                    "content": h.get("content").cloned().unwrap_or(serde_json::Value::Null),
                }));
            }
            _ => {
                out.push(h.clone());
            }
        }
    }
    out
}

fn build_chat_body(
    model: &str,
    sys_prompt: &str,
    history: &[serde_json::Value],
    tools: Option<&Vec<serde_json::Value>>,
) -> serde_json::Value {
    let mut messages = vec![json!({ "role": "system", "content": sys_prompt })];
    messages.extend_from_slice(history);
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    if let Some(t) = tools
        && !t.is_empty()
    {
        body["tools"] = json!(t);
    }
    body
}

fn build_responses_body(
    model: &str,
    sys_prompt: &str,
    history: &[serde_json::Value],
    tools: Option<&Vec<serde_json::Value>>,
) -> serde_json::Value {
    let mut body = json!({
        "model": model,
        "instructions": sys_prompt,
        "input": map_history_to_responses_input(history),
        "stream": true,
    });
    if let Some(t) = tools
        && !t.is_empty()
    {
        body["tools"] = json!(responses_tools(t));
    }
    body
}

fn build_messages_body(
    model: &str,
    sys_prompt: &str,
    history: &[serde_json::Value],
    tools: Option<&Vec<serde_json::Value>>,
) -> serde_json::Value {
    let mut body = json!({
        "model": model,
        "system": sys_prompt,
        "messages": map_history_to_anthropic_messages(history),
        "max_tokens": 8192,
        "stream": true,
    });
    if let Some(t) = tools
        && !t.is_empty()
    {
        body["tools"] = json!(anthropic_tools(t));
    }
    body
}

#[derive(Debug, Default)]
struct ChatStreamState {
    content: String,
    calls: std::collections::BTreeMap<usize, ToolCallChunk>,
}

/// Fold one chat-completions SSE `data:` frame into state. Returns the text
/// token to stream, if any.
fn apply_chat_frame(
    state: &mut ChatStreamState,
    frame: &serde_json::Value,
) -> Option<String> {
    let delta = frame.get("choices")?.get(0)?.get("delta")?;
    let mut token = None;
    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        state.content.push_str(content);
        token = Some(content.to_string());
    }
    if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for call in calls {
            if let Some(index) = call.get("index").and_then(|i| i.as_u64()) {
                let entry = state.calls.entry(index as usize).or_default();
                if let Some(id) = call.get("id").and_then(|i| i.as_str()) {
                    entry.id = id.to_string();
                }
                if let Some(func) = call.get("function") {
                    if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                        entry.name.push_str(name);
                    }
                    if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                        entry.arguments.push_str(args);
                    }
                }
            }
        }
    }
    token
}

#[derive(Debug, Default)]
struct ResponsesStreamState {
    content: String,
    calls: std::collections::BTreeMap<String, ToolCallChunk>,
    order: Vec<String>,
}

/// Key the in-progress function call: prefer the frame's own `item_id`, then
/// the item's `call_id`/`id`, else the most recent call.
fn responses_call_key(
    frame: &serde_json::Value,
    state: &ResponsesStreamState,
) -> Option<String> {
    if let Some(id) = frame.get("item_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        return Some(id.to_string());
    }
    if let Some(item) = frame.get("item") {
        for field in ["call_id", "id"] {
            if let Some(id) = item.get(field).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                return Some(id.to_string());
            }
        }
    }
    state.order.last().cloned()
}

/// Fold one Responses SSE `data:` frame into state. Returns the text token
/// to stream, if any.
fn apply_responses_frame(
    state: &mut ResponsesStreamState,
    frame: &serde_json::Value,
) -> Option<String> {
    match frame.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "response.output_text.delta" => {
            let text = frame.get("delta").and_then(|d| d.as_str()).unwrap_or("");
            state.content.push_str(text);
            if text.is_empty() { None } else { Some(text.to_string()) }
        }
        "response.output_item.added" => {
            let item = frame.get("item")?;
            if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                return None;
            }
            let key = responses_call_key(frame, state)?;
            if key.is_empty() {
                return None;
            }
            let entry = state.calls.entry(key.clone()).or_default();
            entry.id = key.clone();
            if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                entry.name = name.to_string();
            }
            if let Some(args) = item.get("arguments").and_then(|v| v.as_str()) {
                entry.arguments.push_str(args);
            }
            if !state.order.contains(&key) {
                state.order.push(key);
            }
            None
        }
        "response.function_call_arguments.delta" => {
            let key = responses_call_key(frame, state)?;
            if key.is_empty() {
                return None;
            }
            let entry = state.calls.entry(key.clone()).or_default();
            if entry.id.is_empty() {
                entry.id = key.clone();
            }
            if !state.order.contains(&key) {
                state.order.push(key);
            }
            entry
                .arguments
                .push_str(frame.get("delta").and_then(|d| d.as_str()).unwrap_or(""));
            None
        }
        "response.output_item.done" => {
            let item = frame.get("item")?;
            if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                return None;
            }
            let key = responses_call_key(frame, state)?;
            if key.is_empty() {
                return None;
            }
            let entry = state.calls.entry(key.clone()).or_default();
            if entry.id.is_empty() {
                entry.id = key.clone();
            }
            if entry.name.is_empty()
                && let Some(name) = item.get("name").and_then(|v| v.as_str())
            {
                entry.name = name.to_string();
            }
            if entry.arguments.is_empty()
                && let Some(args) = item.get("arguments").and_then(|v| v.as_str())
            {
                entry.arguments = args.to_string();
            }
            if !state.order.contains(&key) {
                state.order.push(key);
            }
            None
        }
        _ => None,
    }
}

#[derive(Debug, Default)]
struct AnthropicStreamState {
    content: String,
    calls: std::collections::BTreeMap<usize, ToolCallChunk>,
}

/// Fold one Anthropic SSE `data:` frame (dispatched on its JSON `type`) into
/// state. Returns the text token to stream, if any.
fn apply_anthropic_frame(
    state: &mut AnthropicStreamState,
    frame: &serde_json::Value,
) -> Option<String> {
    match frame.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "content_block_start" => {
            let index = frame.get("index").and_then(|i| i.as_u64())? as usize;
            let block = frame.get("content_block")?;
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                return None;
            }
            let entry = state.calls.entry(index).or_default();
            entry.id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            entry.name = block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            None
        }
        "content_block_delta" => {
            let index = frame.get("index").and_then(|i| i.as_u64())? as usize;
            let delta = frame.get("delta")?;
            match delta.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text_delta" => {
                    let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    state.content.push_str(text);
                    if text.is_empty() { None } else { Some(text.to_string()) }
                }
                "input_json_delta" => {
                    let entry = state.calls.entry(index).or_default();
                    entry.arguments.push_str(
                        delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or(""),
                    );
                    None
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Normalize accumulated chunks to OpenAI tool_call shape.
fn finalize_tool_calls(chunks: Vec<ToolCallChunk>) -> Vec<serde_json::Value> {
    chunks
        .into_iter()
        .map(|chunk| {
            json!({
                "id": chunk.id,
                "type": "function",
                "function": {
                    "name": chunk.name,
                    "arguments": chunk.arguments,
                }
            })
        })
        .collect()
}

pub(crate) async fn call_llm_streaming(
    settings: &LlmSettings,
    api_key_fallback: &str,
    session_id: &str,
    sys_prompt: &str,
    history: &[serde_json::Value],
    tx: &mpsc::Sender<AgentEvent>,
    tools: Option<Vec<serde_json::Value>>,
    is_native: bool,
) -> anyhow::Result<(String, Vec<serde_json::Value>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let model = settings.model.as_str();
    let url = settings.url.as_str();
    let provider_name: &str =
        crate::config::find_preset(&settings.provider_id).map(|p| p.name).unwrap_or("LLM");
    let effective_key = settings.api_key.as_deref().unwrap_or(api_key_fallback);
    let is_go = settings.is_opencode_go();

    let body = match settings.protocol {
        Protocol::ChatCompletions => build_chat_body(model, sys_prompt, history, tools.as_ref()),
        Protocol::Responses => build_responses_body(model, sys_prompt, history, tools.as_ref()),
        Protocol::AnthropicMessages => {
            build_messages_body(model, sys_prompt, history, tools.as_ref())
        }
    };

    tracing::info!("LLM Call: provider={}, model={}, url={}", provider_name, model, url);

    let candidates = crate::config::llm_candidates(url);
    let mut resp = None;
    let mut last_err = String::new();
    for candidate in &candidates {
        let mut request = client.post(candidate).header("Content-Type", "application/json");
        if is_go {
            request = request
                .header("User-Agent", format!("aria-daemon/{}", env!("CARGO_PKG_VERSION")))
                .header("x-opencode-session", session_id);
            match settings.protocol {
                Protocol::AnthropicMessages => {
                    request = request.header("x-api-key", effective_key);
                }
                _ => {
                    request = request.header("Authorization", format!("Bearer {}", effective_key));
                }
            }
        } else {
            request = request.header("Authorization", format!("Bearer {}", effective_key));
        }
        match request.json(&body).send().await {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(e) => {
                last_err = e.to_string();
                tracing::warn!("LLM endpoint {} unreachable: {}", candidate, last_err);
            }
        }
    }
    let resp = match resp {
        Some(r) => r,
        None => anyhow::bail!(
            "{} error: LLM unreachable (tried {}). Start LiteLLM on port 8000 or set ARIA_LLM_URL. Last error: {}",
            provider_name,
            candidates.join(", "),
            last_err
        ),
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        let mut is_tool_rejection = false;
        if status.is_client_error()
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(err_obj) = json.get("error")
        {
            let err_msg =
                err_obj.get("message").and_then(|m| m.as_str()).unwrap_or_default().to_lowercase();
            let err_type =
                err_obj.get("type").and_then(|t| t.as_str()).unwrap_or_default().to_lowercase();
            let err_code =
                err_obj.get("code").and_then(|c| c.as_str()).unwrap_or_default().to_lowercase();

            if err_msg.contains("tool")
                || err_msg.contains("function")
                || err_type.contains("tool")
                || err_type.contains("function")
                || err_code.contains("tool")
                || err_code.contains("function")
            {
                is_tool_rejection = true;
            }
        }

        if is_tool_rejection {
            anyhow::bail!("TOOL_REJECTION_ERROR {}", text);
        } else {
            anyhow::bail!("{} error {}: {}", provider_name, status, text);
        }
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut seen_json_block = false;
    let mut chat_state = ChatStreamState::default();
    let mut responses_state = ResponsesStreamState::default();
    let mut anthropic_state = AnthropicStreamState::default();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim().to_string();
            buffer = buffer[newline_pos + 1..].to_string();

            if line.is_empty() || line == "data: [DONE]" || line.starts_with("event: ") {
                continue;
            }

            let json_str = line.strip_prefix("data: ").unwrap_or(&line);
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(json_str) else {
                continue;
            };
            let (token, full_text) = match settings.protocol {
                Protocol::ChatCompletions => {
                    let token = apply_chat_frame(&mut chat_state, &frame);
                    (token, chat_state.content.as_str())
                }
                Protocol::Responses => {
                    let token = apply_responses_frame(&mut responses_state, &frame);
                    (token, responses_state.content.as_str())
                }
                Protocol::AnthropicMessages => {
                    let token = apply_anthropic_frame(&mut anthropic_state, &frame);
                    (token, anthropic_state.content.as_str())
                }
            };
            if let Some(content) = token {
                // Streaming safety fix - do not stream JSON to UI in fallback mode!
                if !is_native && (full_text.contains("```json") || full_text.contains("{")) {
                    seen_json_block = true;
                }

                if is_native || !seen_json_block {
                    let _ = tx.send(AgentEvent::Token { content }).await;
                }
            }
        }
    }

    let (full_content, final_tool_calls) = match settings.protocol {
        Protocol::ChatCompletions => {
            let chunks: Vec<ToolCallChunk> = chat_state.calls.into_values().collect();
            (chat_state.content, finalize_tool_calls(chunks))
        }
        Protocol::Responses => {
            let mut chunks = Vec::new();
            for key in &responses_state.order {
                if let Some(chunk) = responses_state.calls.get(key) {
                    chunks.push(chunk.clone());
                }
            }
            (responses_state.content, finalize_tool_calls(chunks))
        }
        Protocol::AnthropicMessages => {
            let chunks: Vec<ToolCallChunk> = anthropic_state.calls.into_values().collect();
            (anthropic_state.content, finalize_tool_calls(chunks))
        }
    };

    if full_content.is_empty() && final_tool_calls.is_empty() {
        tracing::warn!("LLM returned a successful response but NO tokens or tools were found.");
    }

    Ok((full_content, final_tool_calls))
}

fn extract_payment_recipient_and_amount(
    skill: &str,
    args: &serde_json::Value,
) -> Result<(String, f64), String> {
    let capabilities = skill_capabilities(skill).unwrap_or_default();
    if capabilities.hedera_pay {
        let recipient =
            args.get("recipient").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if recipient.is_empty() {
            return Err(format!("Payment proposal for {} is missing recipient.", skill));
        }
        let amount = args
            .get("amount")
            .and_then(json_number_as_f64)
            .ok_or_else(|| format!("Payment proposal for {} is missing valid amount.", skill))?;
        Ok((recipient, amount))
    } else if capabilities.x402_pay {
        let recipient = first_present_arg(
            args,
            &["pay_to", "recipient", "payee", "destination", "paymentAddress"],
        )
        .unwrap_or_default();

        // x402 PaymentRequirements.amount is always in tinybars (i64 string),
        // identical to how x402_vault.rs records it: amount_parsed / 100_000_000.0.
        // We do NOT guess units by magnitude — that heuristic can silently misclassify
        // a large legitimate HBAR amount.
        let amount = first_present_arg(
            args,
            &["amount", "price", "max_amount", "maxAmount", "maxAmountRequired"],
        )
        .and_then(|s| s.parse::<f64>().ok())
        .map(|tinybars| tinybars / 100_000_000.0)
        .unwrap_or(0.0);

        if recipient.is_empty() {
            return Err(format!("Payment proposal for {} is missing recipient account.", skill));
        }
        Ok((recipient, amount))
    } else {
        Err(format!("Skill {} is not a supported payment skill.", skill))
    }
}

pub(crate) fn is_question(text: &str) -> bool {
    let trimmed = text.trim();
    // Rhetorical greetings / offers of help ("How can I help you today?")
    // are answers, not clarifying questions. Classifying them as Ask parks
    // the turn and renders a pointless Submit box duplicating the answer.
    let lower = trimmed.to_lowercase();
    const NON_QUESTIONS: &[&str] = &[
        "how can i help",
        "how may i help",
        "what can i do for you",
        "what can i help",
        "let me know if you need",
        "let me know if i can",
        "anything else i can",
        "happy to help",
        "here to help",
    ];
    if NON_QUESTIONS.iter().any(|p| lower.contains(p)) {
        return false;
    }
    if trimmed.ends_with('?') {
        return true;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == '?' {
            if i + 1 < chars.len() {
                let next = chars[i + 1];
                if next.is_whitespace() || next == '"' || next == '\'' || next == ')' || next == ']' || next == '}' {
                    return true;
                }
            } else {
                return true;
            }
        }
    }
    false
}

