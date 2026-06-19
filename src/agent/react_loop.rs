//! The ReAct execution loop: drives repeated LLM calls, parses structured
//! agent responses (thought/action/ask/final/chat), dispatches skills via
//! SkillManager, and enforces per-skill max_steps / terminal behavior from
//! each skill's manifest.

use std::collections::HashMap;

use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::CONFIG;
use crate::skills::manifest::{ReactConfig, load_manifest};
use crate::skills::paths::skill_dir;

use super::prompt::system_prompt;

pub const MAX_REACT_STEPS: usize = 8;

// ── Agent event / response types ──────────────────────────────────────────────

pub enum AgentEvent {
    Thought(String),
    Action {
        skill: String,
        args: serde_json::Value,
    },
    Observation(String),
    Ask(String),
    /// Streamed token — print immediately, no newline
    Token(String),
    /// Full assembled text after streaming completes (Chat or Final)
    Final(String),
    Chat(String),
    Error(String),
    Done,
}

enum AgentResponseKind {
    Chat(String),
    Thought(String),
    Action {
        skill: String,
        args: serde_json::Value,
    },
    Ask(String),
    Final(String),
}

// ── ReAct loop ────────────────────────────────────────────────────────────────

pub async fn run_react_loop(
    api_key: String,
    mut history: Vec<serde_json::Value>,
    injected_config: HashMap<String, HashMap<String, String>>,
    skills: std::sync::Arc<crate::skills::SkillManager>,
    tx: mpsc::Sender<AgentEvent>,
    user_prompt: String,
) {
    let sys_prompt = system_prompt(&user_prompt);
    let mut skill_fire_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for _ in 0..MAX_REACT_STEPS {
        // Stream the LLM response, emitting Token events as tokens arrive.
        // Returns the full assembled string for parsing once streaming is done.
        let raw = match call_llm_streaming(&api_key, &sys_prompt, &history, &tx).await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx
                    .send(AgentEvent::Error(format!("LLM error: {}", e)))
                    .await;
                let _ = tx.send(AgentEvent::Done).await;
                return;
            }
        };

        let parsed = parse_agent_responses(&raw);

        if parsed.is_empty() {
            // Raw response wasn't structured JSON — treat the streamed output as chat.
            // Tokens already sent; send Done so the REPL prints a newline and stops.
            let _ = tx.send(AgentEvent::Done).await;
            return;
        }

        // Structured response — tokens that were streamed were the JSON blob itself,
        // which isn't user-readable. The REPL suppresses token printing when the
        // response is structured (detected by checking whether we emit any non-Token
        // events). We handle this by not printing a prefix until we know the type.
        let mut should_continue = true;

        for kind in parsed {
            match kind {
                AgentResponseKind::Chat(content) => {
                    let _ = tx.send(AgentEvent::Chat(content)).await;
                    should_continue = false;
                }
                AgentResponseKind::Thought(thought) => {
                    let _ = tx.send(AgentEvent::Thought(thought)).await;
                }
                AgentResponseKind::Action { skill, args } => {
                    let react_meta = load_react_meta(&skill);
                    if let Some(max) = react_meta.max_steps {
                        let count = skill_fire_counts.entry(skill.clone()).or_insert(0);
                        if *count >= max {
                            let _ = tx
                                .send(AgentEvent::Error(format!(
                                    "Skill '{}' exceeded its max_steps limit of {}",
                                    skill, max
                                )))
                                .await;
                            should_continue = false;
                            break;
                        }
                        *count += 1;
                    }

                    let _ = tx
                        .send(AgentEvent::Action {
                            skill: skill.clone(),
                            args: args.clone(),
                        })
                        .await;

                    let mut enriched = args.clone();
                    if let Some(obj) = enriched.as_object_mut() {
                        if let Some(skill_config) = injected_config.get(&skill) {
                            for (k, v) in skill_config {
                                obj.insert(k.clone(), json!(v));
                            }
                        }
                    }

                    let (observation, is_error): (String, bool) =
                        match skills.run_skill_raw(&skill, &enriched).await {
                            Ok(val) => (val.to_string(), false),
                            Err(e) => (e.to_string(), true),
                        };
                    let _ = tx.send(AgentEvent::Observation(observation.clone())).await;

                    if react_meta.terminal && !is_error {
                        let _ = tx.send(AgentEvent::Final(observation)).await;
                        should_continue = false;
                        break;
                    }

                    history.push(json!({ "role": "assistant", "content": raw }));
                    let label = if is_error {
                        "Error from"
                    } else {
                        "Observation from"
                    };
                    history.push(json!({
                        "role": "user",
                        "content": format!("{} {}: {}. If this is an error, consider retrying with corrected args, trying a different skill, or telling the user it failed.", label, skill, observation)
                    }));
                }
                AgentResponseKind::Ask(question) => {
                    let _ = tx.send(AgentEvent::Ask(question)).await;
                    should_continue = false;
                }
                AgentResponseKind::Final(answer) => {
                    let _ = tx.send(AgentEvent::Final(answer)).await;
                    should_continue = false;
                }
            }
        }

        if !should_continue {
            let _ = tx.send(AgentEvent::Done).await;
            return;
        }
    }

    let _ = tx
        .send(AgentEvent::Error(
            "Max steps reached without a final answer.".into(),
        ))
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
        "thought" => Some(AgentResponseKind::Thought(
            v["content"].as_str()?.to_string(),
        )),
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

/// Streams the LLM response token by token, sending each token as
/// `AgentEvent::Token`. Returns the fully assembled content string
/// so the caller can parse it for structured agent responses.
///
/// Tokens are only user-visible for `chat` responses. For structured
/// JSON responses (thought/action/ask/final), the REPL must discard
/// previously streamed tokens once it sees a non-Token event arrive —
/// see the note in `repl.rs` event handler.
async fn call_llm_streaming(
    api_key: &str,
    sys_prompt: &str,
    history: &[serde_json::Value],
    tx: &mpsc::Sender<AgentEvent>,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();

    let mut messages = vec![json!({ "role": "system", "content": sys_prompt })];
    messages.extend_from_slice(history);

    let (url, model, provider_name) = match CONFIG.use_provider {
        crate::config::Provider::OpenRouter => (CONFIG.openrouter_url, CONFIG.openrouter_model, "OpenRouter"),
        crate::config::Provider::Ollama => (CONFIG.ollama_url, CONFIG.ollama_model, "Ollama"),
    };

    let body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });

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
        anyhow::bail!("{} error {}: {}", provider_name, status, text);
    }

    let mut stream = resp.bytes_stream();
    let mut full_content = String::new();
    let mut buffer = String::new();

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
                if let Some(token) = v["choices"][0]["delta"]["content"].as_str() {
                    full_content.push_str(token);
                    // Send token — receiver decides whether to print it
                    let _ = tx.send(AgentEvent::Token(token.to_string())).await;
                }
            }
        }
    }

    Ok(full_content)
}
