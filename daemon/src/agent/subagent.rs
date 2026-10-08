//! Generic `delegate_task` subagent primitive: a bounded, focused child ReAct
//! loop ("single loop spawns workhorses").
//!
//! A parent turn that needs a self-contained, multi-step sub-task done
//! (e.g. research/exploration that would otherwise consume the main turn
//! budget) calls the [`DELEGATE_TOOL_NAME`] tool; the parent loop runs
//! [`run_subagent`] and folds the child's final report back into history as
//! a normal Observation.
//!
//! Safety boundary (hard, v1):
//! - The child toolset ([`build_subagent_tools`]) never contains
//!   `delegate_task` (no recursion), `ask_user` (no user contact), or any
//!   payment / DLT-capable skill (no money movement).
//! - Dispatch additionally refuses those skills deterministically, and the
//!   child always runs with `None` payment/x402 vaults.
//! - Child token events are drained locally, never streamed to the parent GUI
//!   (same shape as [`super::forge::run_forge_subagent`]).

use serde_json::{
    Value,
    json,
};
use tokio::sync::mpsc;

use super::prompt::ASK_TOOL_NAME;
use super::react_loop::AgentEvent;

/// Native tool name the parent loop exposes for spawning a subagent.
pub const DELEGATE_TOOL_NAME: &str = "delegate_task";

/// Max subagent spawns per parent task: bounds cost + abuse surface.
pub const MAX_DELEGATIONS_PER_TASK: usize = 3;

/// Child turn budget: enough for a focused multi-step sub-task, bounded
/// well under the parent's own budget.
const SUBAGENT_MAX_STEPS: usize = 6;

/// OpenAI function tool definition for `delegate_task`, added to every
/// native parent turn by `build_native_tools_with_dlt`.
pub fn delegate_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": DELEGATE_TOOL_NAME,
            "description": "Spawn a focused subagent for a self-contained, multi-step sub-task (e.g. research or exploration) that would otherwise consume the main turn budget. The subagent works with a restricted toolset and returns a concise report you can use. The subagent CANNOT move money, CANNOT ask the user questions, and CANNOT delegate further — do NOT use it for payments or anything needing confirmation; keep payments in the main loop.",
            "parameters": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "A clear, self-contained description of the sub-task and the exact output you want back."
                    },
                    "label": {
                        "type": "string",
                        "description": "Short human-readable label for this delegation (optional)."
                    }
                },
                "required": ["task"]
            }
        }
    })
}

/// What a subagent run produced: either a final `report` or an `error`
/// (e.g. the child LLM was unreachable). The parent folds one of them into
/// history as a normal Observation.
pub struct SubagentResult {
    pub report: String,
    pub error: Option<String>,
}

/// Run a bounded child ReAct loop for `request` and return its final report.
///
/// Mirrors [`super::forge::run_forge_subagent`]: fresh child task id, a
/// drained local event channel (child events never reach the parent GUI),
/// native tool calls with a prompt-fallback retry on tool rejection, and
/// `Ask`/`Final`/`Chat` all ending the loop (no user is reachable from the
/// child — a question becomes the report).
pub async fn run_subagent(
    db: &std::sync::Arc<crate::db::Db>,
    skills: &std::sync::Arc<crate::skills::SkillManager>,
    parent_task_id: &str,
    request: &str,
) -> SubagentResult {
    let child_task_id = db.new_task_id();

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(100);
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            // Child progress is traced at debug; the parent surfaces only
            // the final report as an Observation.
        }
    });

    let api_key = db.get_config("openrouter_api_key").ok().flatten().unwrap_or_default();
    let injected_config = crate::config::RuntimeConfig::load(db).injected_config;
    let agent_did = format!("subagent:{}", parent_task_id);

    let sys = subagent_system_prompt(request);
    let mut history = vec![json!({ "role": "user", "content": request })];
    let mut report = String::new();
    let mut step = 0;
    let mut fallback = false;

    while step < SUBAGENT_MAX_STEPS {
        let settings = crate::config::resolve_llm(db);
        let dlt_enabled = crate::config::dlt_enabled_live(db);
        let tools = if fallback { None } else { Some(build_subagent_tools(request, dlt_enabled)) };
        let sys_for_turn = if fallback { fallback_subagent_prompt(request) } else { sys.clone() };

        let stream_result = super::react_loop::call_llm_streaming(
            &settings,
            &api_key,
            &child_task_id,
            &sys_for_turn,
            &history,
            &tx,
            tools,
            !fallback,
        )
        .await;
        let (raw, tool_calls) = match stream_result {
            Ok(r) => r,
            Err(e) => {
                let s = e.to_string();
                if !fallback && s.starts_with("TOOL_REJECTION_ERROR") {
                    fallback = true;
                    continue;
                }
                return SubagentResult { report: String::new(), error: Some(s) };
            }
        };

        // Parse: native tool calls, or prompt-fallback JSON lines.
        let mut parsed = Vec::new();
        if !fallback {
            if !tool_calls.is_empty() {
                for tc in tool_calls.iter() {
                    if let Some(func) = tc.get("function")
                        && let (Some(name), Some(args_str)) =
                            (func.get("name"), func.get("arguments"))
                    {
                        let name = name.as_str().unwrap_or_default().to_string();
                        let args_text = args_str.as_str().unwrap_or_default();
                        let args: Value =
                            serde_json::from_str(args_text).unwrap_or_else(|_| json!({}));
                        if name == ASK_TOOL_NAME {
                            let q = args
                                .get("question")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(no question)")
                                .to_string();
                            parsed.push(super::react_loop::AgentResponseKind::Ask(q));
                        } else {
                            parsed.push(super::react_loop::AgentResponseKind::Action {
                                skill: name,
                                args,
                            });
                        }
                    }
                }
            } else if !raw.is_empty() {
                if super::react_loop::is_question(&raw) {
                    parsed.push(super::react_loop::AgentResponseKind::Ask(raw.clone()));
                } else {
                    parsed.push(super::react_loop::AgentResponseKind::Final(raw.clone()));
                }
            } else {
                break;
            }
        } else {
            parsed = super::react_loop::parse_agent_responses(&raw);
            if parsed.is_empty() {
                break;
            }
        }

        let mut turn_done = false;
        for kind in parsed {
            match kind {
                super::react_loop::AgentResponseKind::Thought(c) => {
                    history.push(json!({ "role": "assistant", "content": c }));
                }
                super::react_loop::AgentResponseKind::Action { skill, args } => {
                    let (observation, is_error) =
                        dispatch_subagent_action(db, skills, &injected_config, &agent_did, &child_task_id, &skill, &args).await;
                    let label = if is_error { "Error" } else { "Observation" };
                    history.push(json!({
                        "role": "user",
                        "content": format!("{} from {}: {}", label, skill, observation)
                    }));
                }
                super::react_loop::AgentResponseKind::Ask(q) => {
                    // No user is reachable from the child — the question
                    // becomes the report and ends the loop.
                    report = q;
                    turn_done = true;
                    break;
                }
                super::react_loop::AgentResponseKind::Final(f) => {
                    report = f;
                    turn_done = true;
                    break;
                }
                super::react_loop::AgentResponseKind::Chat(c) => {
                    // Same as Final: a subagent cannot converse, so its
                    // closing text is the report.
                    report = c;
                    turn_done = true;
                    break;
                }
            }
        }
        if turn_done {
            break;
        }
        step += 1;
    }

    SubagentResult { report, error: None }
}

/// Execute one child action. Nested delegation, payment skills, and
/// DLT-blocked skills are refused with a deterministic error observation
/// (never executed). Everything else runs with `None` payment/x402 vaults —
/// the money-safety guarantee: even a misclassified skill finds no vault to
/// draw from.
async fn dispatch_subagent_action(
    db: &std::sync::Arc<crate::db::Db>,
    skills: &std::sync::Arc<crate::skills::SkillManager>,
    injected_config: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    agent_did: &str,
    child_task_id: &str,
    skill: &str,
    args: &Value,
) -> (String, bool) {
    if skill == DELEGATE_TOOL_NAME
        || super::react_loop::skill_requires_confirmation(skill)
        || super::react_loop::dlt_blocked_error(db, skill).is_some()
    {
        return (
            format!(
                "Subagent cannot call '{}' (payments and nested delegation are not allowed).",
                skill
            ),
            true,
        );
    }
    let mut enriched = args.clone();
    if let Some(obj) = enriched.as_object_mut()
        && let Some(cfg) = injected_config.get(skill)
    {
        for (k, v) in cfg {
            obj.insert(k.clone(), json!(v));
        }
    }
    match skills
        .run_skill_raw(
            skill,
            &enriched,
            Some(db.clone()),
            None,
            None,
            agent_did.to_string(),
            Some(child_task_id.to_string()),
        )
        .await
    {
        Ok(val) => (val.to_string(), false),
        Err(e) => (e.to_string(), true),
    }
}

// ── Child toolset construction ──────────────────────────────────────────────

/// True when a skill manifest may appear in the subagent toolset: no payment
/// or DLT-move capability, and not one of the structurally excluded tools
/// (nested delegation, user contact, direct pay entry points).
fn subagent_skill_allowed(name: &str, m: &crate::skills::manifest::SkillManifest) -> bool {
    if name == DELEGATE_TOOL_NAME
        || name == ASK_TOOL_NAME
        || name == "hedera_pay"
        || name == "x402_pay"
    {
        return false;
    }
    !(m.capabilities.hedera_pay || m.capabilities.x402_pay)
}

/// Native tools for the child: the full skill list minus delegation, user
/// contact, and anything payment/DLT-capable. The name-based filter is the
/// minimum; the manifest capability filter is the backstop.
fn build_subagent_tools(request: &str, dlt_enabled: bool) -> Vec<Value> {
    let mut tools = Vec::new();
    for t in super::prompt::build_native_tools_with_dlt(request, &None, dlt_enabled) {
        let name = t
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or_default();
        if name == DELEGATE_TOOL_NAME || name == ASK_TOOL_NAME || name == "hedera_pay" || name == "x402_pay" {
            continue;
        }
        let blocked = crate::skills::paths::skill_dir(name)
            .ok()
            .and_then(|dir| crate::skills::manifest::load_manifest(&dir).ok())
            .map(|m| m.capabilities.hedera_pay || m.capabilities.x402_pay)
            .unwrap_or(false);
        if blocked {
            continue;
        }
        tools.push(t);
    }
    tools
}

/// Native system prompt for the child: do the delegated task, then reply
/// with plain text containing the findings/report.
fn subagent_system_prompt(request: &str) -> String {
    format!(
        "You are a focused ARIA subagent. Complete the delegated task and return a concise final report. \
You cannot ask the user questions and you cannot move money or delegate further. \
Use the provided tools as needed; when finished, reply with plain text (no tool call) containing your findings/report. \
Be concise and factual.\n\n== DELEGATED TASK ==\n{}",
        request
    )
}

/// Prompt-fallback variant: subagent system prompt plus the reduced toolset
/// with call schemas, so non-native models emit the same action shapes.
fn fallback_subagent_prompt(request: &str) -> String {
    let mut blocks = Vec::new();
    for m in super::prompt::load_all_skills() {
        if !subagent_skill_allowed(&m.name, &m) {
            continue;
        }
        let args = m.call.args_schema.as_deref().unwrap_or(r#"{"key":"value"}"#);
        blocks.push(format!(
            "- {}: {}\n  call:   {{\"type\":\"action\",\"skill\":\"{}\",\"args\":{}}}",
            m.name, m.description, m.name, args
        ));
    }
    format!(
        "{}\n\nWhen you need tools, respond ONLY with a JSON object on a single line:\n\
{{\"type\":\"action\",\"skill\":\"skill_name\",\"args\":{{...}}}}\n\
To give the final report after all tool steps:\n\
{{\"type\":\"final\",\"content\":\"your report here\"}}\n\n== AVAILABLE SKILLS (subagent toolset: no payments, no delegation, no user contact) ==\n{}",
        subagent_system_prompt(request),
        blocks.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_tool_has_name_and_required_task() {
        let def = delegate_tool_definition();
        let func = def.get("function").expect("tool has function");
        assert_eq!(func.get("name").and_then(|v| v.as_str()), Some(DELEGATE_TOOL_NAME));
        let required = func
            .get("parameters")
            .and_then(|p| p.get("required"))
            .and_then(|r| r.as_array())
            .expect("tool has required");
        assert!(required.iter().any(|v| v.as_str() == Some("task")));
        assert!(
            func.get("parameters")
                .and_then(|p| p.get("properties"))
                .and_then(|p| p.get("task"))
                .is_some()
        );
    }

    #[test]
    fn subagent_tools_exclude_delegation_contact_and_payments() {
        for dlt_enabled in [true, false] {
            let tools = build_subagent_tools("research test query", dlt_enabled);
            for t in &tools {
                let name = t
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default();
                assert_ne!(name, DELEGATE_TOOL_NAME, "dlt={}", dlt_enabled);
                assert_ne!(name, ASK_TOOL_NAME, "dlt={}", dlt_enabled);
                assert_ne!(name, "hedera_pay", "dlt={}", dlt_enabled);
                assert_ne!(name, "x402_pay", "dlt={}", dlt_enabled);
            }
        }
    }

    #[test]
    fn subagent_skill_allowed_blocks_pay_capabilities() {
        let mut m = crate::skills::manifest::SkillManifest {
            name: "pay.test".to_string(),
            version: "0.1.0".to_string(),
            description: "d".to_string(),
            triggers: vec![],
            capabilities: crate::skills::manifest::Capabilities {
                hedera_pay: true,
                ..Default::default()
            },
            call: Default::default(),
            react: Default::default(),
            config: Default::default(),
            display: Default::default(),
        };
        assert!(!subagent_skill_allowed("pay.test", &m));
        m.capabilities.hedera_pay = false;
        m.capabilities.x402_pay = true;
        assert!(!subagent_skill_allowed("pay.test", &m));
        m.capabilities.x402_pay = false;
        assert!(subagent_skill_allowed("pay.test", &m));
        assert!(!subagent_skill_allowed(DELEGATE_TOOL_NAME, &m));
        assert!(!subagent_skill_allowed(ASK_TOOL_NAME, &m));
    }
}
