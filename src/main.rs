use std::env;
use std::fs;
use std::io::{self, Write};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod agent;
mod config;
mod crypto;
mod db;
mod identity;
mod skills;

use crate::config::RuntimeConfig;
use crate::db::Db;
use crate::identity::IdentityVault;
use crate::identity::file::FileVault;
use crate::skills::SkillManager;

#[derive(serde::Deserialize)]
struct DaemonRequest {
    task: String,
    #[serde(rename = "Type")]
    skills_type: Option<String>,
}

fn print_help() {
    println!("ARIA — Governed Agent Runtime v0.6");
    println!();
    println!("Usage: aria [COMMAND]");
    println!();
    println!("Commands:");
    println!("  daemon    Run headless TCP service (default)");
    println!("  install   Install systemd user service for auto-start");
    println!("  help      Show this help");
    println!();
    println!("Examples:");
    println!("  aria");
    println!("  aria daemon");
    println!("  aria install");
}

fn install_service() -> anyhow::Result<()> {
    let os = env::consts::OS;
    println!("Installing ARIA Daemon as a startup service on {}...", os);

    match os {
        "linux" => {
            let exe_path = env::current_exe()?;
            let user_home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
            let systemd_dir = user_home.join(".config/systemd/user");
            fs::create_dir_all(&systemd_dir)?;

            let service_content = format!(r#"[Unit]
Description=ARIA Governed Agent Daemon
After=network.target

[Service]
ExecStart="{}" daemon
Restart=always
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#, exe_path.display());

            let service_path = systemd_dir.join("aria-daemon.service");
            fs::write(&service_path, service_content)?;

            println!("✓ Service file created: {:?}", service_path);
            println!("  Run this to enable and start:");
            println!("  systemctl --user enable --now aria-daemon");
        }
        "windows" => { println!("Windows auto-start not yet implemented."); }
        _ => { return Err(anyhow::anyhow!("Unsupported OS for auto-start installation")); }
    }
    Ok(())
}

fn bootstrap_db() -> anyhow::Result<Db> {
    let db = Db::new()?;
    // Generate identity if missing
    db.ensure_identity("did:aria:jayesh")?;
    Ok(db)
}

fn prompt_api_key(db: &Db) -> anyhow::Result<String> {
    if crate::config::CONFIG.use_provider == crate::config::Provider::Ollama {
        return Ok(db.get_config("openrouter_api_key").unwrap_or_default().unwrap_or_default());
    }
    match db.get_config("openrouter_api_key") {
        Ok(Some(key)) => Ok(key),
        Ok(None) => {
            println!("No OpenRouter API key found.");
            print!("Enter your API key: ");
            io::stdout().flush()?;
            let mut api_key = String::new();
            io::stdin().read_line(&mut api_key)?;
            let api_key = api_key.trim().to_string();
            if api_key.is_empty() { anyhow::bail!("API key cannot be empty"); }
            db.set_config("openrouter_api_key", &api_key)?;
            println!("✓ API key saved.");
            Ok(api_key)
        }
        Err(e) => Err(anyhow::anyhow!("Failed to check API key: {}", e)),
    }
}

async fn run_daemon() -> anyhow::Result<()> {
    let db = Arc::new(bootstrap_db()?);
    let api_key = prompt_api_key(&db)?;
    let runtime_cfg = RuntimeConfig::load(&db);
    let skills = Arc::new(SkillManager::new()?);

    // Initialize HAL Vault with auto-probing
    let (did, pub_key) = db.get_identity()?.ok_or_else(|| anyhow::anyhow!("Identity missing from DB"))?;
    let (vault, level) = crate::identity::initialize_vault(did, pub_key).await?;
    info!("Identity HAL initialized (Mode: {:?})", level);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:5005").await?;
    info!("ARIA Daemon listening on 127.0.0.1:5005");

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (mut socket, addr) = result?;
                info!("New connection from {}", addr);

                let api_key = api_key.clone();
                let runtime_cfg = runtime_cfg.clone();
                let skills = skills.clone();
                let vault = vault.clone();

                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let n = match socket.read(&mut buffer).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };

                    let req: DaemonRequest = match serde_json::from_slice(&buffer[..n]) {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = socket.write_all(format!("Invalid JSON: {}\n", e).as_bytes()).await;
                            return;
                        }
                    };

                    info!("Received task: {}", req.task);

                    let db = match Db::new() {
                        Ok(d) => d,
                        Err(e) => {
                            let _ = socket.write_all(format!("DB Connect Error: {}\n", e).as_bytes()).await;
                            return;
                        }
                    };

                    // Sign the global task chain link via Vault HAL
                    let (link_hash, _) = match db.get_task_link_info("temp_id", &req.task) {
                        Ok(info) => info,
                        Err(_) => ("".to_string(), "".to_string()),
                    };
                    let task_chain_sig = vault.sign(link_hash.as_bytes()).await.unwrap_or_default();

                    let task_id: String = match db.create_task(&vault.did(), "tcp", &req.task, &task_chain_sig) {
                        Ok(id) => id,
                        Err(e) => {
                            let _ = socket.write_all(format!("DB Error: {}\n", e).as_bytes()).await;
                            return;
                        }
                    };

                    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
                    let history = vec![serde_json::json!({ "role": "user", "content": req.task.clone() })];

                    tokio::spawn(async move {
                        crate::agent::react_loop::run_react_loop(
                            api_key, history, runtime_cfg.injected_config, skills, tx, req.task, req.skills_type,
                        ).await;
                    });

                    let mut last_action: Option<(String, serde_json::Value)> = None;

                    while let Some(event) = rx.recv().await {
                        match &event {
                            crate::agent::react_loop::AgentEvent::Action { skill, args } => {
                                last_action = Some((skill.clone(), args.clone()));
                            }
                            crate::agent::react_loop::AgentEvent::Observation { content } |
                            crate::agent::react_loop::AgentEvent::Error { content } => {
                                let success = matches!(event, crate::agent::react_loop::AgentEvent::Observation { .. });
                                if let Some((skill, args)) = last_action.take() {
                                    // Compute and sign audit entry via Vault HAL
                                    if let Ok((step, prev_hash, timestamp)) = db.get_next_step_info(&task_id) {
                                        let input_hash = crypto::sha256_hex_str(&args.to_string());
                                        let result_hash = crypto::sha256_hex_str(content);
                                        let chain_hash = crypto::compute_chain_hash(&prev_hash, &step.to_string(), &skill, &input_hash, &result_hash, &timestamp);
                                        
                                        match vault.sign(chain_hash.as_bytes()).await {
                                            Ok(sig) => {
                                                let _ = db.log_task_step(&task_id, &vault.did(), &skill, &args.to_string(), content, success, &sig);
                                            },
                                            Err(e) => warn!("HAL Signing failed: {}", e),
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }

                        let json = serde_json::to_string(&event).unwrap_or_default();
                        if socket.write_all(format!("{}\n", json).as_bytes()).await.is_err() { break; }
                    }

                    let _ = db.seal_task(&task_id, crate::db::TaskStatus::Done);
                });
            }
            _ = signal::ctrl_c() => {
                info!("Shutdown signal received. Closing ARIA Daemon...");
                break;
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("daemon");

    match command {
        "install" => return install_service(),
        "daemon" => {
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            let log_dir = home.join(".aria");
            std::fs::create_dir_all(&log_dir).ok();

            let file_appender = tracing_appender::rolling::daily(&log_dir, "daemon.log");
            let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

            tracing_subscriber::registry()
                .with(fmt::layer())
                .with(fmt::layer().with_writer(non_blocking))
                .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
                .init();

            info!("Logging initialized. Logs saved to: {:?}", log_dir.join("daemon.log"));
            return run_daemon().await;
        }
        "help" | "--help" | "-h" => { print_help(); Ok(()) }
        _ => {
            tracing_subscriber::registry()
                .with(fmt::layer())
                .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
                .init();
            run_daemon().await
        }
    }
}
