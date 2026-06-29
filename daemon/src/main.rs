use std::env;
use std::fs;
use std::io::{self, Write};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod agent;
mod config;
mod crypto;
mod db;
mod skills;

use crate::config::RuntimeConfig;
use crate::db::Db;
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
            let user_home =
                dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
            let systemd_dir = user_home.join(".config/systemd/user");
            fs::create_dir_all(&systemd_dir)?;

            let service_content = format!(
                r#"[Unit]
Description=ARIA Governed Agent Daemon
After=network.target

[Service]
ExecStart="{}" daemon
Restart=always
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#,
                exe_path.display()
            );

            let service_path = systemd_dir.join("aria-daemon.service");
            fs::write(&service_path, service_content)?;

            println!("✓ Service file created: {:?}", service_path);
            println!("  Run this to enable and start:");
            println!("  systemctl --user enable --now aria-daemon");
        }
        "windows" => {
            println!("Windows auto-start not yet implemented.");
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Unsupported OS for auto-start installation"
            ));
        }
    }
    Ok(())
}

fn bootstrap_db() -> anyhow::Result<Db> {
    let db = Db::new()?;
    // Phase 2: generate real Ed25519 keypair on first run (no-op if already exists)
    db.ensure_identity("did:aria:jayesh")?;
    Ok(db)
}

fn prompt_api_key(db: &Db) -> anyhow::Result<String> {
    if crate::config::CONFIG.use_provider == crate::config::Provider::Ollama {
        return Ok(db
            .get_config("openrouter_api_key")
            .unwrap_or_default()
            .unwrap_or_default());
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
            if api_key.is_empty() {
                anyhow::bail!("API key cannot be empty");
            }
            db.set_config("openrouter_api_key", &api_key)?;
            println!("✓ API key saved.");
            Ok(api_key)
        }
        Err(e) => Err(anyhow::anyhow!("Failed to check API key: {}", e)),
    }
}

async fn run_daemon() -> anyhow::Result<()> {
    let db = std::sync::Arc::new(bootstrap_db()?);
    let api_key = prompt_api_key(&db)?;
    let runtime_cfg = RuntimeConfig::load(&db);
    let skills = std::sync::Arc::new(SkillManager::new()?);

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

                tokio::spawn(async move {
                    let db = match Db::new() {
                        Ok(d) => d,
                        Err(e) => {
                            let _ = socket.write_all(format!("DB Connect Error: {}\n", e).as_bytes()).await;
                            return;
                        }
                    };
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

                    // Create task in db
                    let task_id: String = match db.create_task("did:aria:jayesh", "tcp", &req.task) {
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
                            api_key,
                            history,
                            runtime_cfg.injected_config,
                            skills,
                            tx,
                            req.task,
                            req.skills_type,
                        ).await;
                    });

                    let mut last_action: Option<(String, serde_json::Value)> = None;

                    while let Some(event) = rx.recv().await {
                        // Log actions and observations to the task chain
                        match &event {
                            crate::agent::react_loop::AgentEvent::Action { skill, args } => {
                                last_action = Some((skill.clone(), args.clone()));
                            }
                            crate::agent::react_loop::AgentEvent::Observation { content } => {
                                if let Some((skill, args)) = last_action.take() {
                                    if let Ok(db) = Db::new() {
                                        let _ = db.log_task_step(
                                            &task_id,
                                            "did:aria:jayesh",
                                            &skill,
                                            &args.to_string(),
                                            content,
                                            true,
                                        );
                                    }
                                }
                            }
                            crate::agent::react_loop::AgentEvent::Error { content } => {
                                if let Some((skill, args)) = last_action.take() {
                                    if let Ok(db) = Db::new() {
                                        let _ = db.log_task_step(
                                            &task_id,
                                            "did:aria:jayesh",
                                            &skill,
                                            &args.to_string(),
                                            content,
                                            false,
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }

                        let json = serde_json::to_string(&event).unwrap_or_default();
                        if socket.write_all(format!("{}\n", json).as_bytes()).await.is_err() {
                            break;
                        }
                    }

                    // Seal task
                    if let Ok(db) = Db::new() {
                        let _ = db.seal_task(&task_id, crate::db::TaskStatus::Done);
                    }
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
            tracing_subscriber::registry()
                .with(fmt::layer())
                .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
                .init();
            return run_daemon().await;
        }
        "help" | "--help" | "-h" => {
            print_help();
            return Ok(());
        }
        _ => {
            tracing_subscriber::registry()
                .with(fmt::layer())
                .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
                .init();
            return run_daemon().await;
        }
    }
}
