//! CLI REPL — thin client for local developer use only.
//! Production callers go through the daemon's MCP/HTTP interface.
//! Users do NOT talk to the daemon through this; it's a dev tool.

use serde_json::json;
use std::io::{self, BufRead, Write};
use tokio::sync::mpsc;

use crate::agent::react_loop::{AgentEvent, run_react_loop};
use crate::config::RuntimeConfig;
use crate::db::{Db, TaskStatus};

const AGENT_DID: &str = "did:aria:jayesh";

pub async fn run(
    db: &Db,
    api_key: &str,
    cfg: RuntimeConfig,
    skills: std::sync::Arc<crate::skills::SkillManager>,
) -> anyhow::Result<()> {
    println!("ARIA v0.6 — Governed Agent Runtime");
    if cfg.brave_api_key.is_some() {
        println!("Search: Brave API + SearXNG fallback");
    } else {
        println!("Search: SearXNG ({})", cfg.searxng_url);
    }
    println!("Dev REPL — tasks are logged and audited. /help for commands.");
    println!();

    let stdin = io::stdin();
    let api_key = api_key.to_string();
    let injected_config = cfg.injected_config.clone();
    let mut llm_history: Vec<serde_json::Value> = Vec::new();

    loop {
        print!("▸ Task: ");
        io::stdout().flush()?;

        let mut cmd = String::new();
        match stdin.lock().read_line(&mut cmd) {
            Ok(0) => break Ok(()),
            Ok(_) => {}
            Err(e) => {
                eprintln!("Input error: {}", e);
                break Ok(());
            }
        }
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            continue;
        }

        match cmd.as_str() {
            "/quit" | "/exit" | "/q" => break Ok(()),
            "/help" | "/h" => {
                println!("  /help    — show this message");
                println!("  /quit    — exit ARIA");
                println!("  /clear   — clear screen");
                println!("  /key     — show redacted API key");
                println!("  /config  — show runtime config");
                println!("  /tasks   — list recent tasks");
                continue;
            }
            "/clear" => {
                print!("\x1B[2J\x1B[H");
                io::stdout().flush()?;
                continue;
            }
            "/key" => {
                let end = api_key.len().saturating_sub(4);
                println!(
                    "  Key: {}...{}",
                    &api_key[..4.min(api_key.len())],
                    &api_key[end..]
                );
                continue;
            }
            "/config" => {
                println!("  Injected config keys:");
                for (skill_name, configs) in &injected_config {
                    println!("    [ {} ]", skill_name);
                    for (k, v) in configs {
                        let display =
                            if k.contains("key") || k.contains("secret") || k.contains("token") {
                                if v.is_empty() { "(not set)".to_string() } else { "(set)".to_string() }
                            } else {
                                v.clone()
                            };
                        println!("      {}: {}", k, display);
                    }
                }
                continue;
            }
            "/tasks" => {
                match db.list_tasks(10) {
                    Ok(tasks) => {
                        println!("  Recent tasks:");
                        for (id, source, status, steps, created, sealed) in tasks {
                            println!(
                                "  [{}] {} | {} steps | {} → {}{}",
                                &id[..8], source, steps, created,
                                status,
                                if sealed.is_empty() { String::new() } else { format!(" (sealed {})", &sealed[..16]) }
                            );
                        }
                    }
                    Err(e) => eprintln!("Failed to list tasks: {}", e),
                }
                continue;
            }
            _ => {}
        }

        // Open a new audited task for this prompt
        let task_id = match db.create_task(AGENT_DID, "cli", &cmd) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("Failed to create task: {}", e);
                continue;
            }
        };
        println!("  task {}", &task_id[..8]);

        llm_history.push(json!({ "role": "user", "content": cmd.clone() }));

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
        let mut response_kind: Option<bool> = None;
        let mut last_action: Option<(String, serde_json::Value)> = None;
        let mut task_failed = false;

        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Token(t) => {
                    token_buf.push_str(&t);
                    if response_kind.is_none() && token_buf.len() >= 8 {
                        let is_json = token_buf.trim_start().starts_with('{');
                        response_kind = Some(is_json);
                        if !is_json {
                            println!();
                            print!("◂ Aria: {}", token_buf);
                            io::stdout().flush().ok();
                            prefix_printed = true;
                        }
                    } else if response_kind == Some(false) {
                        print!("{}", t);
                        io::stdout().flush().ok();
                    }
                }

                AgentEvent::Chat(content) => {
                    if !prefix_printed { println!(); println!("◂ Aria: {}", content); }
                    else { println!(); }
                    token_buf.clear();
                    llm_history.push(json!({ "role": "assistant", "content": content }));
                }

                AgentEvent::Final(f) => {
                    if !prefix_printed { println!(); println!("◂ Aria: {}", f); }
                    else { println!(); }
                    token_buf.clear();
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
                    last_action = Some((skill, args));
                }

                AgentEvent::Observation(o) => {
                    println!("📋  {}", o);
                    if let Some((skill, args)) = last_action.take() {
                        db.log_task_step(
                            &task_id,
                            AGENT_DID,
                            &skill,
                            &args.to_string(),
                            &o,
                            true,
                        ).ok();
                    }
                }

                AgentEvent::Error(e) => {
                    token_buf.clear();
                    task_failed = true;
                    // Log the failed step if we have a pending action
                    if let Some((skill, args)) = last_action.take() {
                        db.log_task_step(
                            &task_id,
                            AGENT_DID,
                            &skill,
                            &args.to_string(),
                            &e,
                            false,
                        ).ok();
                    }
                    eprintln!("✗ error:  {}", e);
                }

                AgentEvent::Done => {
                    if !token_buf.is_empty() && !prefix_printed {
                        println!();
                        println!("◂ Aria: {}", token_buf.trim());
                        llm_history.push(json!({ "role": "assistant", "content": token_buf.trim() }));
                        token_buf.clear();
                    }
                    break;
                }
            }
        }

        handle.await.ok();

        // Seal the task
        let status = if task_failed { TaskStatus::Failed } else { TaskStatus::Done };
        db.seal_task(&task_id, status).ok();

        println!();
    }
}
