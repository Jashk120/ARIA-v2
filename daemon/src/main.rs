use std::env;
use std::fs;
use std::io::{self, Write};
use tokio::signal;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod agent;
mod config;
mod db;
mod repl;
mod skills;

use crate::config::RuntimeConfig;
use crate::db::Db;

fn print_help() {
    println!("ARIA — Governed Agent Runtime v0.5");
    println!();
    println!("Usage: aria [COMMAND]");
    println!();
    println!("Commands:");
    println!("  (none)    Start interactive chat REPL");
    println!("  daemon    Run headless in background (for systemd)");
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
    db.ensure_stub_identity()?;
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
    let _db = bootstrap_db()?;
    info!("ARIA Daemon running headless...");

    tokio::select! {
        _ = async {
            loop {
                info!("Daemon heartbeat: waiting for triggers...");
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            }
        } => {},
        _ = signal::ctrl_c() => {
            info!("Shutdown signal received. Closing ARIA Daemon...");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("");

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
        _ => {}
    }

    let db = bootstrap_db()?;
    let api_key = prompt_api_key(&db)?;
    let runtime_cfg = RuntimeConfig::load(&db);
    let skills = std::sync::Arc::new(crate::skills::SkillManager::new()?);
    repl::run(&db, &api_key, runtime_cfg, skills).await
}
