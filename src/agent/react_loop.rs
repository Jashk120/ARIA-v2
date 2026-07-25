//! The ReAct execution loop: drives repeated LLM calls, parses structured
//! agent responses (thought/action/ask/final/chat), dispatches skills via
//! SkillManager, and enforces per-skill max_steps / terminal behavior from
//! each skill's manifest.

use std::collections::HashMap;

use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use super::prompt::system_prompt;
use crate::config::CONFIG;
use crate::skills::manifest::{
    Capabilities,
    ReactConfig,
    load_manifest,
};
use crate::skills::paths::{
    skill_dir,
    wasm_path,
};

pub const MAX_REACT_STEPS: usize = 8;

// ── Agent event / response types ──────────────────────────────────────────────

#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AgentEvent {
    Thought {
        content: String,
    },
    Action {
        skill: String,
        args: serde_json::Value,
    },
    Observation {
        content: String,
    },
    Ask {
        content: String,
    },
    /// Streamed token — print immediately, no newline
    Token {
        content: String,
    },
    /// Full assembled text after streaming completes (Chat or Final)
    Final {
        content: String,
    },
    Chat {
        content: String,
    },
    Error {
        content: String,
    },
    Done,
}

enum AgentResponseKind {
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
                if let Some(error) = payment_proposal_error(&skill, &args) {
                    let _ = db.clear_pending_action(&task_id);
                    let _ = tx.send(AgentEvent::Error { content: error }).await;
                    let _ = tx.send(AgentEvent::Done).await;
                    return Ok(());
                }

                let current_fingerprint = payment_fingerprint(&skill, &args, &injected_config);
                if stored_fingerprint != current_fingerprint {
                    let refreshed = pending_payment_action(&skill, args.clone(), &injected_config);
                    let question = payment_confirmation_message(&skill, &args);
                    let _ = tx.send(AgentEvent::Ask { content: question }).await;

                    history.push(json!({
                        "role": "assistant",
                        "content": format!(
                            "Payment confirmation refreshed because the pending transaction fingerprint changed: {}",
                            payment_action_summary(&skill, &args)
                        )
                    }));

                    let history_json = serde_json::to_string(&history).unwrap_or_default();
                    let pending_json = serde_json::to_string(&refreshed).unwrap_or_default();
                    let _ = db.save_awaiting_confirmation(&task_id, &history_json, &pending_json);

                    let _ = tx.send(AgentEvent::Done).await;
                    return Ok(());
                }

                let _ = db.clear_pending_action(&task_id);

                let _ =
                    tx.send(AgentEvent::Action { skill: skill.clone(), args: args.clone() }).await;

                let mut enriched = args.clone();
                if let Some(obj) = enriched.as_object_mut()
                    && let Some(skill_config) = injected_config.get(&skill)
                {
                    for (k, v) in skill_config {
                        obj.insert(k.clone(), json!(v));
                    }
                }

                let (observation, is_error): (String, bool) = match skills
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
                    Ok(val) => (val.to_string(), false),
                    Err(e) => (e.to_string(), true),
                };
                let _ = tx.send(AgentEvent::Observation { content: observation.clone() }).await;

                if is_error {
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
            ConfirmationDecision::Denied => {
                let _ = db.clear_pending_action(&task_id);
                let _ = tx
                    .send(AgentEvent::Final {
                        content: format!("Cancelled — {} was not executed.", skill),
                    })
                    .await;
                let _ = tx.send(AgentEvent::Done).await;
                return Ok(());
            }
            ConfirmationDecision::ContinueConversation => {
                let _ = db.clear_pending_action(&task_id);
                history.push(json!({
                    "role": "user",
                    "content": pending_payment_resume_context(&pending, &user_prompt)
                }));
            }
        }
    }

    while step < MAX_REACT_STEPS {
        let (url, model, provider_name) = match CONFIG.use_provider {
            crate::config::Provider::OpenRouter => {
                (CONFIG.openrouter_url, CONFIG.openrouter_model, "OpenRouter")
            }
            crate::config::Provider::Ollama => (CONFIG.ollama_url, CONFIG.ollama_model, "Ollama"),
        };

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
        let sys_prompt = system_prompt(&user_prompt, skills_type.clone(), is_native);
        let tools = if is_native {
            Some(crate::agent::prompt::build_native_tools(&user_prompt, &skills_type))
        } else {
            None
        };

        tracing::info!("System Prompt sent to LLM: \n{}", sys_prompt);

        // Stream the LLM response.
        let stream_result = call_llm_streaming(
            &api_key,
            &sys_prompt,
            &history,
            &tx,
            tools,
            (url, model, provider_name),
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
                        parsed.push(AgentResponseKind::Action { skill: name, args });
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
                    // Already Native, so plain text is just Final
                    parsed.push(AgentResponseKind::Final(raw.clone()));
                }
            } else {
                let _ = tx.send(AgentEvent::Done).await;
                return Ok(());
            }
        }

        let mut should_continue = true;
        let mut executed_tools = false;

        for kind in parsed {
            match kind {
                AgentResponseKind::Chat(content) => {
                    let _ = tx.send(AgentEvent::Chat { content }).await;
                    should_continue = false;
                }
                AgentResponseKind::Thought(thought) => {
                    let _ = tx.send(AgentEvent::Thought { content: thought }).await;
                }
                AgentResponseKind::Action { skill, args } => {
                    executed_tools = true;
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

                        let question = payment_confirmation_message(&skill, &args);
                        let _ = tx.send(AgentEvent::Ask { content: question }).await;

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
                        let _ =
                            db.save_awaiting_confirmation(&task_id, &history_json, &pending_json);

                        should_continue = false;
                        break;
                    }
                    let _ = tx
                        .send(AgentEvent::Action { skill: skill.clone(), args: args.clone() })
                        .await;

                    let mut enriched = args.clone();
                    if let Some(obj) = enriched.as_object_mut()
                        && let Some(skill_config) = injected_config.get(&skill)
                    {
                        for (k, v) in skill_config {
                            obj.insert(k.clone(), json!(v));
                        }
                    }

                    let (observation, is_error): (String, bool) = match skills
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
                        Ok(val) => (val.to_string(), false),
                        Err(e) => (e.to_string(), true),
                    };
                    let _ = tx.send(AgentEvent::Observation { content: observation.clone() }).await;

                    if react_meta.terminal && !is_error {
                        let _ = tx.send(AgentEvent::Final { content: observation }).await;
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
                    let _ = tx.send(AgentEvent::Ask { content: question }).await;
                    should_continue = false;
                }
                AgentResponseKind::Final(answer) => {
                    let _ = tx.send(AgentEvent::Final { content: answer }).await;
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

/// Skills that can move real money (direct HBAR transfer or x402) must not
/// fire without an explicit human "yes" — the LLM proposing the call is not
/// sufficient authorization on its own.
fn skill_requires_confirmation(skill: &str) -> bool {
    skill_capabilities(skill)
        .map(|capabilities| capabilities.hedera_pay || capabilities.x402_pay)
        .unwrap_or(false)
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
    lines.push("Reply:".to_string());
    lines.push("• yes — execute exactly this transaction".to_string());
    lines.push("• no — cancel".to_string());
    lines.push("• anything else — ask a question or modify the transaction".to_string());
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

fn parse_agent_responses(raw: &str) -> Vec<AgentResponseKind> {
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

#[derive(Debug, Default)]
struct ToolCallChunk {
    id: String,
    name: String,
    arguments: String,
}

async fn call_llm_streaming(
    api_key: &str,
    sys_prompt: &str,
    history: &[serde_json::Value],
    tx: &mpsc::Sender<AgentEvent>,
    tools: Option<Vec<serde_json::Value>>,
    provider_info: (&str, &str, &str),
    is_native: bool,
) -> anyhow::Result<(String, Vec<serde_json::Value>)> {
    let client = reqwest::Client::new();
    let (url, model, provider_name) = provider_info;

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
        body.as_object_mut().unwrap().insert("tools".to_string(), json!(t));
    }

    tracing::info!("LLM Call: provider={}, model={}, url={}", provider_name, model, url);

    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

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
    let mut full_content = String::new();
    let mut buffer = String::new();
    let mut tool_calls_map: std::collections::BTreeMap<usize, ToolCallChunk> =
        std::collections::BTreeMap::new();
    let mut seen_json_block = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim().to_string();
            buffer = buffer[newline_pos + 1..].to_string();

            if line.is_empty() || line == "data: [DONE]" {
                continue;
            }

            let json_str = line.strip_prefix("data: ").unwrap_or(&line);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str)
                && let Some(choices) = v.get("choices")
                && let Some(delta) = choices.get(0).and_then(|c| c.get("delta"))
            {
                // 1. Content accumulation & streaming
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    full_content.push_str(content);

                    // Streaming safety fix - do not stream JSON to UI in fallback mode!
                    if !is_native
                        && (full_content.contains("```json") || full_content.contains("{"))
                    {
                        seen_json_block = true;
                    }

                    if is_native || !seen_json_block {
                        let _ = tx.send(AgentEvent::Token { content: content.to_string() }).await;
                    }
                }

                // 2. Tool calls accumulation (never streamed to UI)
                if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for call in calls {
                        if let Some(index) = call.get("index").and_then(|i| i.as_u64()) {
                            let index = index as usize;
                            let entry = tool_calls_map.entry(index).or_default();

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
            }
        }
    }

    let mut final_tool_calls = Vec::new();
    for (_, chunk) in tool_calls_map {
        final_tool_calls.push(json!({
            "id": chunk.id,
            "type": "function",
            "function": {
                "name": chunk.name,
                "arguments": chunk.arguments,
            }
        }));
    }

    if full_content.is_empty() && final_tool_calls.is_empty() {
        tracing::warn!("LLM returned a successful response but NO tokens or tools were found.");
    }

    Ok((full_content, final_tool_calls))
}
