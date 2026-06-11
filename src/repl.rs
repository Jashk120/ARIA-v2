use std::io::{self, BufRead, Write};
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::{CONFIG, RuntimeConfig};
use crate::db::Db;

const AGENT_DID: &str = "did:aria:jayesh";
const MAX_REACT_STEPS: usize = 8;

// ── Manifest meta (for prompt building only) ──────────────────────────────────

#[derive(serde::Deserialize)]
struct SkillMeta {
    name:        String,
    description: String,
    #[serde(default)]
    call:        CallMeta,
    #[serde(default)]
    react:       ReactMeta,
}

#[derive(serde::Deserialize, Default)]
struct CallMeta {
    /// JSON shape the LLM should pass as args — shown verbatim in prompt
    args_schema:   Option<String>,
    /// JSON shape the skill returns — so LLM can reason about observations
    output_schema: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct ReactMeta {
    /// Max times this skill may fire in one turn (default: unlimited within MAX_REACT_STEPS)
    max_steps: Option<usize>,
    /// If true, skill output is the final answer — skip LLM synthesis pass
    #[serde(default)]
    terminal:  bool,
}

// ── System prompt ─────────────────────────────────────────────────────────────

fn system_prompt() -> String {
    let skills = build_skills_prompt();
    format!(r#"You are ARIA, a governed agent runtime. You are helpful, concise, and precise.

You decide whether a user message needs tool use or is just a conversation.

== WHEN TO USE TOOLS ==
Use tools ONLY when the user explicitly wants to interact with files, data, or the system.
- "search for X", "find info on Y", "look up Z" → use search.web skill
- "read my resume", "find files", "rate this document" → use file skills
- "hi", "how are you", "explain X", "what is Y" → just reply normally, NO tools

== TOOL USE FORMAT ==
When you need tools, respond ONLY with a JSON object on a single line. No other text.

To think before acting:
{{"type":"thought","content":"your reasoning here"}}

To call a skill (use the exact args schema shown per skill below):
{{"type":"action","skill":"skill_name","args":{{...}}}}

To ask the user for confirmation or clarification:
{{"type":"ask","content":"your question here"}}

To give the final answer after all tool steps:
{{"type":"final","content":"your response here"}}

For normal conversation (no tools needed):
{{"type":"chat","content":"your response here"}}

{}

== RULES ==
- Always emit a thought before every action
- Use the exact args schema defined per skill — do not invent keys
- After receiving an observation, either act again or emit final
- Keep thoughts short and practical
- Final answers should be friendly and summarize what was done"#, skills)
}

// ── Skills prompt builder ─────────────────────────────────────────────────────

fn build_skills_prompt() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    let root = exe
        .parent().and_then(|p| p.parent()).and_then(|p| p.parent())
        .unwrap_or_else(|| std::path::Path::new("."));
    let skills_dir = root.join("skills");

    let mut blocks: Vec<String> = Vec::new();

    if let Ok(categories) = std::fs::read_dir(&skills_dir) {
        for cat in categories.flatten() {
            if !cat.path().is_dir() { continue; }
            if let Ok(entries) = std::fs::read_dir(cat.path()) {
                for entry in entries.flatten() {
                    let manifest_path = entry.path().join("manifest.toml");
                    if let Ok(text) = std::fs::read_to_string(&manifest_path) {
                        if let Ok(m) = toml::from_str::<SkillMeta>(&text) {
                            blocks.push(format_skill_block(&m));
                        }
                    }
                }
            }
        }
    }

    if blocks.is_empty() {
        return "== AVAILABLE SKILLS ==\n(no skills found — build WASM binaries first)".to_string();
    }

    format!("== AVAILABLE SKILLS ==\n{}", blocks.join("\n\n"))
}

fn format_skill_block(m: &SkillMeta) -> String {
    let args_example = m.call.args_schema.as_deref()
        .unwrap_or(r#"{"key":"value"}"#);
    let call_line = format!(
        r#"  call:   {{"type":"action","skill":"{}","args":{}}}"#,
        m.name, args_example
    );

    let mut lines = vec![
        format!("- {}: {}", m.name, m.description),
        call_line,
    ];

    if let Some(out) = &m.call.output_schema {
        lines.push(format!("  output: {}", out));
    }

    if m.react.terminal {
        lines.push("  note:   result is returned directly as the final answer".to_string());
    }
    if let Some(n) = m.react.max_steps {
        lines.push(format!("  note:   may fire at most {} time(s) per turn", n));
    }

    lines.join("\n")
}

// ── Agent event / response types ──────────────────────────────────────────────

enum AgentEvent {
    Thought(String),
    Action { skill: String, args: serde_json::Value },
    Observation(String),
    Ask(String),
    Final(String),
    Chat(String),
    Error(String),
    Done,
}

enum AgentResponseKind {
    Chat(String),
    Thought(String),
    Action { skill: String, args: serde_json::Value },
    Ask(String),
    Final(String),
}

// ── REPL entry point ──────────────────────────────────────────────────────────

pub async fn run(db: &Db, api_key: &str, cfg: RuntimeConfig) -> anyhow::Result<()> {
    let history = db.get_history(AGENT_DID, 50)?;
    let mut history_reversed = history.clone();
    history_reversed.reverse();

    let mut llm_history: Vec<serde_json::Value> = history_reversed
        .iter()
        .map(|(dir, content)| {
            let role = if dir == "sent" { "user" } else { "assistant" };
            json!({ "role": role, "content": content })
        })
        .collect();

    println!("ARIA v0.5 — Governed Agent Runtime");
    if cfg.brave_api_key.is_some() {
        println!("Search: Brave API + SearXNG fallback");
    } else {
        println!("Search: SearXNG ({})", cfg.searxng_url);
    }
    println!("Type a message, or /help for commands. Ctrl+C or /quit to exit.");
    println!();

    let stdin = io::stdin();
    let api_key = api_key.to_string();
    let searxng_url = cfg.searxng_url.clone();
    let brave_api_key = cfg.brave_api_key.clone();

    loop {
        print!("▸ You: ");
        io::stdout().flush()?;

        let mut cmd = String::new();
        match stdin.lock().read_line(&mut cmd) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => { eprintln!("Input error: {}", e); break; }
        }
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() { continue; }

        match cmd.as_str() {
            "/quit" | "/exit" | "/q" => break,
            "/help" | "/h" => {
                println!("  /help    — show this message");
                println!("  /quit    — exit ARIA");
                println!("  /clear   — clear screen");
                println!("  /key     — show redacted API key");
                println!("  /config  — show runtime config");
                continue;
            }
            "/clear" => { print!("\x1B[2J\x1B[H"); io::stdout().flush()?; continue; }
            "/key" => {
                let end = api_key.len().saturating_sub(4);
                println!("  Key: {}...{}", &api_key[..4.min(api_key.len())], &api_key[end..]);
                continue;
            }
            "/config" => {
                println!("  searxng_url:   {}", searxng_url);
                println!("  brave_api_key: {}", if brave_api_key.is_some() { "set" } else { "not set" });
                continue;
            }
            _ => {}
        }

        db.save_message(AGENT_DID, "sent", &cmd).ok();
        llm_history.push(json!({ "role": "user", "content": cmd.clone() }));

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(32);
        let key = api_key.clone();
        let history_snap = llm_history.clone();
        let s_url = searxng_url.clone();
        let b_key = brave_api_key.clone();

        let handle = tokio::spawn(async move {
            run_react_loop(key, history_snap, s_url, b_key, tx).await;
        });

        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Done => break,
                AgentEvent::Error(e) => eprintln!("✗ error:  {}", e),
                AgentEvent::Thought(t) => println!("💭  {}", t),
                AgentEvent::Action { skill, args } => {
                    let readable = crate::skills::describe_action(&skill, &args);
                    println!("⚡  {}", readable);
                }
                AgentEvent::Observation(o) => println!("📋  {}", o),
                AgentEvent::Ask(q) => { println!(); println!("◂ Aria: {}", q); }
                AgentEvent::Final(f) => {
                    println!(); println!("◂ Aria: {}", f);
                    db.save_message(AGENT_DID, "received", &f).ok();
                    llm_history.push(json!({ "role": "assistant", "content": f }));
                }
                AgentEvent::Chat(c) => {
                    println!(); println!("◂ Aria: {}", c);
                    db.save_message(AGENT_DID, "received", &c).ok();
                    llm_history.push(json!({ "role": "assistant", "content": c }));
                }
            }
        }

        handle.await.ok();
        println!();
    }

    println!("Goodbye.");
    Ok(())
}

// ── ReAct loop ────────────────────────────────────────────────────────────────

async fn run_react_loop(
    api_key: String,
    mut history: Vec<serde_json::Value>,
    searxng_url: String,
    brave_api_key: Option<String>,
    tx: mpsc::Sender<AgentEvent>,
) {
    let sys_prompt = system_prompt();
    // Per-skill fire counts for manifest.react.max_steps enforcement
    let mut skill_fire_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for _ in 0..MAX_REACT_STEPS {
        let raw = match call_llm(&api_key, &sys_prompt, &history).await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(format!("LLM error: {}", e))).await;
                let _ = tx.send(AgentEvent::Done).await;
                return;
            }
        };

        let parsed = parse_agent_responses(&raw);

        if parsed.is_empty() {
            let display = extract_content_field(&raw).unwrap_or(raw.clone());
            let _ = tx.send(AgentEvent::Chat(display)).await;
            let _ = tx.send(AgentEvent::Done).await;
            return;
        }

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
                    // Enforce per-skill max_steps from manifest
                    let react_meta = load_react_meta(&skill);
                    if let Some(max) = react_meta.max_steps {
                        let count = skill_fire_counts.entry(skill.clone()).or_insert(0);
                        if *count >= max {
                            let _ = tx.send(AgentEvent::Error(format!(
                                "Skill '{}' exceeded its max_steps limit of {}",
                                skill, max
                            ))).await;
                            should_continue = false;
                            break;
                        }
                        *count += 1;
                    }

                    let _ = tx.send(AgentEvent::Action { skill: skill.clone(), args: args.clone() }).await;

                    // Inject runtime config — bypass db-based enrich_args
                    let mut enriched = args.clone();
                    if let Some(obj) = enriched.as_object_mut() {
                        obj.insert("searxng_url".to_string(), json!(searxng_url));
                        if let Some(ref key) = brave_api_key {
                            obj.insert("brave_api_key".to_string(), json!(key));
                        }
                    }

                    let (observation, is_error) = match crate::skills::run_skill_raw(&skill, &enriched).await {
                        Ok(val) => (val.to_string(), false),
                        Err(e)  => (e.to_string(), true),
                    };
                    let _ = tx.send(AgentEvent::Observation(observation.clone())).await;

                    // If terminal, emit the observation directly as Final — skip LLM synthesis
                    if react_meta.terminal && !is_error {
                        let _ = tx.send(AgentEvent::Final(observation)).await;
                        should_continue = false;
                        break;
                    }


                    history.push(json!({ "role": "assistant", "content": raw }));
                    let label = if is_error { "Error from" } else { "Observation from" };
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

    let _ = tx.send(AgentEvent::Error("Max steps reached without a final answer.".into())).await;
    let _ = tx.send(AgentEvent::Done).await;
}

// ── Manifest helpers ──────────────────────────────────────────────────────────

fn skill_manifest_dir(name: &str) -> Option<std::path::PathBuf> {
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    if parts.len() != 2 { return None; }
    let (action, category) = (parts[0], parts[1]);
    let exe = std::env::current_exe().ok()?;
    let root = exe.parent()?.parent()?.parent()?;
    Some(root.join("skills").join(category).join(format!("{}.{}", action, category)))
}

fn load_react_meta(skill: &str) -> ReactMeta {
    skill_manifest_dir(skill)
        .and_then(|dir| std::fs::read_to_string(dir.join("manifest.toml")).ok())
        .and_then(|text| toml::from_str::<SkillMeta>(&text).ok())
        .map(|m| m.react)
        .unwrap_or_default()
}

// ── Parsing ───────────────────────────────────────────────────────────────────

fn parse_single(line: &str) -> Option<AgentResponseKind> {
    let cleaned = line.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let v: serde_json::Value = serde_json::from_str(cleaned).ok()?;

    match v["type"].as_str()? {
        "chat"    => Some(AgentResponseKind::Chat(v["content"].as_str()?.to_string())),
        "thought" => Some(AgentResponseKind::Thought(v["content"].as_str()?.to_string())),
        "ask"     => Some(AgentResponseKind::Ask(v["content"].as_str()?.to_string())),
        "final"   => Some(AgentResponseKind::Final(v["content"].as_str()?.to_string())),
        "action"  => Some(AgentResponseKind::Action {
            skill: v["skill"].as_str()?.to_string(),
            args:  v["args"].clone(),
        }),
        _ => None,
    }
}

fn parse_agent_responses(raw: &str) -> Vec<AgentResponseKind> {
    raw.lines()
        .filter_map(|line| parse_single(line.trim()))
        .collect()
}

fn extract_content_field(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    v["content"].as_str().map(|c| c.to_string())
}

// ── LLM call ──────────────────────────────────────────────────────────────────

async fn call_llm(api_key: &str, sys_prompt: &str, history: &[serde_json::Value]) -> anyhow::Result<String> {
    let client = reqwest::Client::new();

    let mut messages = vec![json!({ "role": "system", "content": sys_prompt })];
    messages.extend_from_slice(history);

    let body = json!({
        "model": CONFIG.default_model,
        "messages": messages,
    });

    let resp = client
        .post(CONFIG.openrouter_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenRouter error {}: {}", status, text);
    }

    let json: serde_json::Value = resp.json().await?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No content in response"))?
        .to_string();

    Ok(content)
}