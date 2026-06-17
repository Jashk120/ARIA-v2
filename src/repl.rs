use std::io::{self, BufRead, Write};
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::RuntimeConfig;
use crate::db::Db;
use crate::agent::react_loop::{run_react_loop, AgentEvent};

const AGENT_DID: &str = "did:aria:jayesh";

pub async fn run(
    db: &Db,
    api_key: &str,
    cfg: RuntimeConfig,
    skills: std::sync::Arc<crate::skills::SkillManager>,
) -> anyhow::Result<()> {
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
    let injected_config = cfg.injected_config.clone();

    loop {
        print!("▸ You: ");
        io::stdout().flush()?;

        let mut cmd = String::new();
        match stdin.lock().read_line(&mut cmd) {
            Ok(0) => break Ok(()),
            Ok(_) => {}
            Err(e) => { eprintln!("Input error: {}", e); break Ok(()); }
        }
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() { continue; }

        match cmd.as_str() {
            "/quit" | "/exit" | "/q" => break Ok(()),
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
                println!("  Injected config keys:");
                for (skill_name, configs) in &injected_config {
                    println!("    [ {} ]", skill_name);
                    for (k, v) in configs {
                        // Mask secrets (keys containing "key", "secret", "token")
                        let display = if k.contains("key") || k.contains("secret") || k.contains("token") {
                            if v.is_empty() { "(not set)".to_string() } else { "(set)".to_string() }
                        } else {
                            v.clone()
                        };
                        println!("      {}: {}", k, display);
                    }
                }
                continue;
            }
            _ => {}
        }

        db.save_message(AGENT_DID, "sent", &cmd).ok();
        llm_history.push(json!({ "role": "user", "content": cmd.clone() }));

        // Large channel — JSON responses can be 50-100 tokens; 32 caused deadlock.
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(512);
        let key = api_key.clone();
        let history_snap = llm_history.clone();
        let ic = injected_config.clone();
        let user_prompt = cmd.clone();

        let sm = std::sync::Arc::clone(&skills);
        let handle = tokio::spawn(async move {
            run_react_loop(key, history_snap, ic, sm, tx, user_prompt).await;
        });

        let mut token_buf = String::new();
        let mut prefix_printed = false;
        // None = undecided, Some(true) = JSON, Some(false) = chat
        let mut response_kind: Option<bool> = None;

        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Token(t) => {
                    token_buf.push_str(&t);

                    // Wait for 8 chars before deciding — first token may be just `{`
                    if response_kind.is_none() && token_buf.len() >= 8 {
                        let is_json = token_buf.trim_start().starts_with('{');
                        response_kind = Some(is_json);
                        if !is_json {
                            // Flush buffered chars now that we know it's chat
                            println!();
                            print!("◂ Aria: {}", token_buf);
                            io::stdout().flush().ok();
                            prefix_printed = true;
                        }
                    } else if response_kind == Some(false) {
                        // Chat confirmed — stream token live
                        print!("{}", t);
                        io::stdout().flush().ok();
                    }
                    // response_kind == Some(true): JSON — stay silent, let structured events handle output
                }

                AgentEvent::Chat(content) => {
                    if !prefix_printed {
                        println!();
                        println!("◂ Aria: {}", content);
                    } else {
                        println!();
                    }
                    token_buf.clear();
                    db.save_message(AGENT_DID, "received", &content).ok();
                    llm_history.push(json!({ "role": "assistant", "content": content }));
                }

                AgentEvent::Final(f) => {
                    if !prefix_printed {
                        println!();
                        println!("◂ Aria: {}", f);
                    } else {
                        println!();
                    }
                    token_buf.clear();
                    db.save_message(AGENT_DID, "received", &f).ok();
                    llm_history.push(json!({ "role": "assistant", "content": f }));
                }

                AgentEvent::Ask(q) => {
                    token_buf.clear();
                    println!();
                    println!("◂ Aria: {}", q);
                }

                AgentEvent::Thought(t) => {
                    token_buf.clear();
                    println!("💭  {}", t);
                }

                AgentEvent::Action { skill, args } => {
                    token_buf.clear();
                    let readable = crate::skills::describe_action(&skill, &args);
                    println!("⚡  {}", readable);
                }

                AgentEvent::Observation(o) => {
                    println!("📋  {}", o);
                }

                AgentEvent::Error(e) => {
                    token_buf.clear();
                    eprintln!("✗ error:  {}", e);
                }

                AgentEvent::Done => {
                    // Fallback: undecided or chat with <8 chars total
                    if !token_buf.is_empty() && !prefix_printed {
                        println!();
                        println!("◂ Aria: {}", token_buf.trim());
                        db.save_message(AGENT_DID, "received", token_buf.trim()).ok();
                        llm_history.push(json!({ "role": "assistant", "content": token_buf.trim() }));
                        token_buf.clear();
                    }
                    break;
                }
            }
        }

        handle.await.ok();
        println!();
    }
}