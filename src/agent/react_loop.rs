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
    ReactConfig,
    load_manifest,
};
use crate::skills::paths::skill_dir;

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
    payment_vault: std::sync::Arc<crate::payments::direct::PaymentVault>,
    x402_vault: Option<std::sync::Arc<crate::payments::x402_vault::X402PaymentVault>>,
    agent_did: String,
    task_id: String,
) {
    let mut skill_fire_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    let mut step = 0;
    let mut force_fallback_this_turn = false;

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
                return;
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
                return;
            }
        } else {
            if !tool_calls.is_empty() {
                for tc in tool_calls.iter() {
                    if let Some(func) = tc.get("function") {
                        if let (Some(name), Some(args_str)) =
                            (func.get("name"), func.get("arguments"))
                        {
                            let name = name.as_str().unwrap_or_default().to_string();
                            let args_text = args_str.as_str().unwrap_or_default();
                            let args: serde_json::Value =
                                serde_json::from_str(args_text).unwrap_or_else(|_| json!({}));
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
                    // Already Native, so plain text is just Final
                    parsed.push(AgentResponseKind::Final(raw.clone()));
                }
            } else {
                let _ = tx.send(AgentEvent::Done).await;
                return;
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

                    let _ = tx
                        .send(AgentEvent::Action { skill: skill.clone(), args: args.clone() })
                        .await;

                    let mut enriched = args.clone();
                    if let Some(obj) = enriched.as_object_mut() {
                        if let Some(skill_config) = injected_config.get(&skill) {
                            for (k, v) in skill_config {
                                obj.insert(k.clone(), json!(v));
                            }
                        }
                    }

                    let (observation, is_error): (String, bool) = match skills
                        .run_skill_raw(
                            &skill,
                            &enriched,
                            Some(db.clone()),
                            Some(payment_vault.clone()),
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
                            if let Some(f) = tc.get("function") {
                                if let Some(n) = f.get("name") {
                                    return n.as_str().unwrap_or_default() == skill;
                                }
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
            return;
        }

        step += 1;
    }

    let _ = tx
        .send(AgentEvent::Error { content: "Max steps reached without a final answer.".into() })
        .await;
    let _ = tx.send(AgentEvent::Done).await;
}

fn load_react_meta(skill: &str) -> ReactConfig {
    skill_dir(skill)
        .ok()
        .and_then(|dir| load_manifest(&dir).ok())
        .map(|m| m.react)
        .unwrap_or_default()
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

    if let Some(t) = tools {
        if !t.is_empty() {
            body.as_object_mut().unwrap().insert("tools".to_string(), json!(t));
        }
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
        if status.is_client_error() {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(err_obj) = json.get("error") {
                    let err_msg = err_obj
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or_default()
                        .to_lowercase();
                    let err_type = err_obj
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_lowercase();
                    let err_code = err_obj
                        .get("code")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default()
                        .to_lowercase();

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
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(choices) = v.get("choices") {
                    if let Some(delta) = choices.get(0).and_then(|c| c.get("delta")) {
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
                                let _ = tx
                                    .send(AgentEvent::Token { content: content.to_string() })
                                    .await;
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
                                        if let Some(name) =
                                            func.get("name").and_then(|n| n.as_str())
                                        {
                                            entry.name.push_str(name);
                                        }
                                        if let Some(args) =
                                            func.get("arguments").and_then(|a| a.as_str())
                                        {
                                            entry.arguments.push_str(args);
                                        }
                                    }
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
