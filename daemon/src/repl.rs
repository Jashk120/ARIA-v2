use std::io::{self, BufRead, Write};
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::CONFIG;
use crate::db::Db;

const AGENT_DID: &str = "did:aria:jayesh";
const MAX_REACT_STEPS: usize = 8;

const SYSTEM_PROMPT: &str = r#"You are ARIA, a governed agent runtime. You are helpful, concise, and precise.

You decide whether a user message needs tool use or is just a conversation.

== WHEN TO USE TOOLS ==
Use tools ONLY when the user explicitly wants to interact with files, data, or the system.
- "read my resume", "find files", "rate this document" → use tools
- "hi", "how are you", "explain X", "what is Y" → just reply normally, NO tools

== TOOL USE FORMAT ==
When you need tools, respond ONLY with a JSON object. No other text.

To think before acting:
{"type":"thought","content":"your reasoning here"}

To call a skill:
{"type":"action","skill":"skill_name","args":{"key":"value"}}

To ask the user for confirmation or clarification:
{"type":"ask","content":"your question here"}

To give the final answer after all tool steps:
{"type":"final","content":"your response here"}

For normal conversation (no tools needed):
{"type":"chat","content":"your response here"}

== AVAILABLE SKILLS (STUBBED) ==
- find_files: args: {"path": "~/some/dir/", "pattern": "*.pdf"}
- read_file: args: {"path": "/absolute/path/to/file.txt"}
- rate: args: {"text": "document content here"}
- summarize: args: {"text": "document content here"}
- notify: args: {"message": "text", "channel": "terminal"}

== RULES ==
- Always think before acting (emit a thought first)
- For file operations, use find_files first, then confirm with the user before reading
- Never call a skill without a preceding thought
- Keep thoughts short and practical
- Final answers should be friendly and summarize what was done"#;

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

pub async fn run(db: &Db, api_key: &str) -> anyhow::Result<()> {
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
    println!("Type a message, or /help for commands. Ctrl+C or /quit to exit.");
    println!();

    let stdin = io::stdin();
    let api_key = api_key.to_string();

    loop {
        print!("▸ You: ");
        io::stdout().flush()?;

        let mut cmd = String::new();
        match stdin.lock().read_line(&mut cmd) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("Input error: {}", e);
                break;
            }
        }
        let cmd = cmd.trim().to_string();

        if cmd.is_empty() {
            continue;
        }

        match cmd.as_str() {
            "/quit" | "/exit" | "/q" => break,
            "/help" | "/h" => {
                println!("  /help   — show this message");
                println!("  /quit   — exit ARIA");
                println!("  /clear  — clear screen");
                println!("  /key    — show redacted API key");
                continue;
            }
            "/clear" => {
                print!("\x1B[2J\x1B[H");
                io::stdout().flush()?;
                continue;
            }
            "/key" => {
                let end = api_key.len().saturating_sub(4);
                println!("  Key: {}...{}", &api_key[..4.min(api_key.len())], &api_key[end..]);
                continue;
            }
            _ => {}
        }

        db.save_message(AGENT_DID, "sent", &cmd).ok();
        llm_history.push(json!({ "role": "user", "content": cmd.clone() }));

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(32);
        let key = api_key.clone();
        let history_snap = llm_history.clone();

        let handle = tokio::spawn(async move {
            run_react_loop(key, history_snap, tx).await;
        });

        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Done => break,
                AgentEvent::Error(e) => {
                    eprintln!("✗ error:  {}", e);
                }
                AgentEvent::Thought(t) => {
                    println!("💭  {}", t);
                }
                AgentEvent::Action { skill, args } => {
                    let readable = describe_action(&skill, &args);
                    println!("⚡  {}", readable);
                }
                AgentEvent::Observation(o) => {
                    let clean = o.trim_start_matches("[STUB] ");
                    println!("📋  {}", clean);
                }
                AgentEvent::Ask(q) => {
                    println!();
                    println!("◂ Aria: {}", q);
                }
                AgentEvent::Final(f) => {
                    println!();
                    println!("◂ Aria: {}", f);
                    db.save_message(AGENT_DID, "received", &f).ok();
                    llm_history.push(json!({ "role": "assistant", "content": f }));
                }
                AgentEvent::Chat(c) => {
                    println!();
                    println!("◂ Aria: {}", c);
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

async fn run_react_loop(
    api_key: String,
    mut history: Vec<serde_json::Value>,
    tx: mpsc::Sender<AgentEvent>,
) {
    for _ in 0..MAX_REACT_STEPS {
        let raw = match call_llm(&api_key, &history).await {
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
                    let _ = tx.send(AgentEvent::Action { skill: skill.clone(), args: args.clone() }).await;
                    let observation = run_stub_skill(&skill, &args);
                    let _ = tx.send(AgentEvent::Observation(observation.clone())).await;
                    history.push(json!({ "role": "assistant", "content": raw }));
                    history.push(json!({
                        "role": "user",
                        "content": format!("Observation from {}: {}", skill, observation)
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
            args: v["args"].clone(),
        }),
        _ => None,
    }
}

fn parse_agent_responses(raw: &str) -> Vec<AgentResponseKind> {
    raw.lines()
        .filter_map(|line| parse_single(line.trim()))
        .collect()
}

fn describe_action(skill: &str, args: &serde_json::Value) -> String {
    match skill {
        "find_files" => {
            let path = args["path"].as_str().unwrap_or("~");
            let pattern = args["pattern"].as_str().unwrap_or("*");
            format!("Searching {} for {}", path, pattern)
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("file");
            format!("Reading {}", path)
        }
        "rate"      => "Rating document…".to_string(),
        "summarize" => "Summarizing document…".to_string(),
        "notify"    => {
            let msg = args["message"].as_str().unwrap_or("(no message)");
            format!("Sending notification: {}", msg)
        }
        other => format!("Running skill: {}", other),
    }
}

fn extract_content_field(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    v["content"].as_str().map(|c| c.to_string())
}

fn run_stub_skill(skill: &str, args: &serde_json::Value) -> String {
    match skill {
        "find_files" => {
            let path = args["path"].as_str().unwrap_or("~/");
            let pattern = args["pattern"].as_str().unwrap_or("*");
            format!(
                "[STUB] Searched {} for '{}' — found: resume_john.pdf, resume_jane.pdf, notes.txt",
                path, pattern
            )
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("unknown");
            format!("[STUB] Read {} — skill not yet implemented.", path)
        }
        "rate"      => "[STUB] Rated document — score: 7.4/10.".to_string(),
        "summarize" => "[STUB] Summarized document — skill not yet implemented.".to_string(),
        "notify"    => {
            let msg = args["message"].as_str().unwrap_or("");
            format!("[STUB] Would notify: '{}'", msg)
        }
        other => format!("[STUB] Unknown skill '{}'", other),
    }
}

async fn call_llm(api_key: &str, history: &[serde_json::Value]) -> anyhow::Result<String> {
    let client = reqwest::Client::new();

    let mut messages = vec![json!({ "role": "system", "content": SYSTEM_PROMPT })];
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
