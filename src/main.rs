use std::io::{
    self,
    Write,
};
use std::sync::Arc;
use std::{
    env,
    fs,
};

use tokio::io::{
    AsyncReadExt,
    AsyncWriteExt,
};
use tracing::{
    info,
    warn,
};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{
    EnvFilter,
    fmt,
};

use crate::db::TaskStatus;

mod agent;
mod config;
mod crypto;
mod db;
mod identity;
mod payments;
mod skills;

use crate::config::RuntimeConfig;
use crate::db::Db;
use crate::identity::IdentityVault;
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
            return Err(anyhow::anyhow!("Unsupported OS for auto-start installation"));
        }
    }
    Ok(())
}

fn bootstrap_db() -> anyhow::Result<Db> {
    let db = Db::new()?;
    // Generate identity if missing
    db.ensure_identity("did:aria:jayesh")?;
    Ok(db)
}

/// Attempt to build an X402PaymentVault from environment; returns None on any failure.
/// This lets the daemon start without Hedera credentials — x402 just won't work.
fn build_x402_vault(
    db: Arc<crate::db::Db>,
) -> Option<Arc<crate::payments::x402_vault::X402PaymentVault>> {
    use std::str::FromStr;

    use hiero_sdk::{
        AccountId,
        Client,
        PrivateKey,
    };

    let account_id_str = std::env::var("HEDERA_ACCOUNT_ID").ok()?;
    let private_key_str = std::env::var("HEDERA_PRIVATE_KEY").ok()?;
    if private_key_str.trim().is_empty() {
        return None;
    }

    let operator_id = AccountId::from_str(&account_id_str).ok()?;
    let private_key = PrivateKey::from_str_ecdsa(&private_key_str).ok()?;

    let network = std::env::var("HEDERA_NETWORK").unwrap_or_else(|_| "testnet".to_string());
    let client = match network.as_str() {
        "mainnet" => Client::for_mainnet(),
        "previewnet" => Client::for_previewnet(),
        _ => Client::for_testnet(),
    };
    client.set_operator(operator_id, private_key.clone());

    let facilitator_url = std::env::var("X402_FACILITATOR_URL")
        .unwrap_or_else(|_| "https://x402.org/facilitator".to_string());

    Some(Arc::new(crate::payments::x402_vault::X402PaymentVault::new(
        client,
        operator_id,
        private_key,
        db,
        facilitator_url,
    )))
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
    let db = Arc::new(bootstrap_db()?);
    let api_key = prompt_api_key(&db)?;
    let runtime_cfg = RuntimeConfig::load(&db);
    let payment_vault = std::sync::Arc::new(crate::payments::direct::PaymentVault::from_env()?);

    let x402_vault: Option<Arc<crate::payments::x402_vault::X402PaymentVault>> =
        build_x402_vault(db.clone());

    let skills = Arc::new(SkillManager::new()?);

    let (did, pub_key) =
        db.get_identity()?.ok_or_else(|| anyhow::anyhow!("Identity missing from DB"))?;
    let (vault, level) = crate::identity::initialize_vault(did, pub_key).await?;
    let vault = Arc::new(vault);
    info!("Identity HAL initialized (Mode: {:?})", level);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:5005").await?;
    info!("ARIA Daemon listening on 127.0.0.1:5005");

    loop {
        tokio::select! {
                                    result = listener.accept() => {
                                        let (mut socket, _addr) = result?;

                                        // Clone references from outer context instead of re-instantiating
                                        let db = db.clone();
                                        let api_key = api_key.clone();
                                        let runtime_cfg = runtime_cfg.clone();
                                        let skills = skills.clone();
                                        let vault = vault.clone();
                                        let payment_vault = payment_vault.clone();
                                        let x402_vault = x402_vault.clone();

                                        tokio::spawn(async move {
                                            let mut buffer = vec![0u8; 16384]; // Increased buffer capacity
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

                                            let new_task_id = db.new_task_id();

                                            // FIX 1: Pass the actual `new_task_id` to fetch correct link info
                                            let (link_hash, created_at) = match db.get_task_link_info(&new_task_id, &req.task) {
                                                Ok(info) => info,
                                                Err(_) => ("".to_string(), "".to_string()),
                                            };

                                            let task_chain_sig = vault.sign(link_hash.as_bytes()).await.unwrap_or_default();

                                            let task_id = match db.create_task(
                                                &new_task_id,
                                                &vault.did(),
                                                "tcp",
                                                &req.task,
                                                &link_hash,
                                                &task_chain_sig,
                                                &created_at,
                                            ) {
                                                Ok(id) => id,
                                                Err(e) => {
                                                    let _ = socket.write_all(format!("DB Error: {}\n", e).as_bytes()).await;
                                                    return;
                                                }
                                            };

                                            let (tx, mut rx) = tokio::sync::mpsc::channel(100);
                                            let history = vec![serde_json::json!({ "role": "user", "content": req.task.clone() })];

                                            let db_for_loop = db.clone();
                                            let did_for_loop = vault.did();
                                            let task_id_for_loop = task_id.clone();

                                            let handle = tokio::spawn(async move {
                                                crate::agent::react_loop::run_react_loop(
                                                    api_key, history, runtime_cfg.injected_config, skills, tx, req.task, req.skills_type,
                                                    db_for_loop, payment_vault, x402_vault, did_for_loop, task_id_for_loop
                                                ).await
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
                                                        if let Some((skill, args)) = last_action.take()
                                                            && let Ok((step, prev_hash, timestamp)) = db.get_next_step_info(&task_id) {
                                                                let input_hash = crypto::sha256_hex_str(&args.to_string());
                                                                let result_hash = crypto::sha256_hex_str(content);
                                                                let chain_hash = crypto::compute_chain_hash(
                                                                    &prev_hash, &step.to_string(), &skill, &input_hash, &result_hash, &timestamp
                                                                );

                                                                match vault.sign(chain_hash.as_bytes()).await {
                                                                    Ok(sig) => {
                                                                        if let Err(e) = db.log_task_step(&task_id, &vault.did(), &skill, &args.to_string(), content, success, &sig) {
                                                                            warn!("Failed to log step to DB: {}", e);
                                                                        }
                                                                    },
                                                                    Err(e) => warn!("HAL Signing failed: {}", e),
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

                                          let status = match handle.await {
            Ok(Ok(())) => TaskStatus::Done,
            Ok(Err(e)) => {
                warn!("Task failed: {}", e);
                TaskStatus::Failed
            }
            Err(e) => {
                warn!("Task panicked: {}", e);
                TaskStatus::Failed
            }
        };

             if let Err(e) = db.seal_task(&task_id, status) {
            warn!("Failed to seal task {}: {}", task_id, e);
        }


                                        });
                                    }
                                    _ = tokio::signal::ctrl_c() => {
                                        info!("Shutdown signal received. Closing ARIA Daemon...");
                                        break;
                                    }
                                }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
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
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => {
            tracing_subscriber::registry()
                .with(fmt::layer())
                .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
                .init();
            run_daemon().await
        }
    }
}
