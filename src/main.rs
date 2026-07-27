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
    #[serde(default)]
    task: String,
    #[serde(rename = "Type")]
    skills_type: Option<String>,
    /// If set and the task is awaiting confirmation, resume it with `task`
    /// treated as the human's yes/no reply instead of a new instruction.
    task_id: Option<String>,
    /// Read-only daemon query, short-circuited before any task/ReAct-loop
    /// dispatch: "query_budget", "query_holds", "query_allowlist", or
    /// "query_wallet_balance". When set, `task` is ignored.
    #[serde(default)]
    query: Option<String>,
    /// Mutating daemon endpoint, short-circuited the same way `query` is:
    /// currently only "mutate_allowlist". When set, `task` and `query` are
    /// ignored.
    #[serde(default)]
    mutate: Option<String>,
    /// For `mutate: "mutate_allowlist"`: "add" or "remove".
    action: Option<String>,
    /// For `mutate: "mutate_allowlist"`: the account to add/remove.
    account: Option<String>,
}

/// Response envelope for the read-only query endpoints. Mirrors the
/// internally-tagged `{"type": ..., ...}` shape `AgentEvent` already uses on
/// this socket, so clients parse both response families the same way.
#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum QueryResponse {
    QueryBudget {
        agent_did: String,
        per_task_cap: Option<f64>,
        per_day_cap: Option<f64>,
        committed_spend_24h: f64,
        held_spend: f64,
        /// None when there's no per-day cap configured (i.e. unlimited).
        remaining_budget: Option<f64>,
    },
    QueryHolds {
        agent_did: String,
        holds: Vec<crate::db::PaymentHoldRecord>,
    },
    QueryAllowlist {
        agent_did: String,
        accounts: Vec<String>,
    },
    QueryWalletBalance {
        agent_did: String,
        account_id: String,
        balance_hbar: f64,
    },
    MutateAllowlist {
        agent_did: String,
        action: String,
        account: String,
        /// true if the allowlist actually changed (account added, or
        /// account was present and got removed); false for a no-op (e.g.
        /// adding an already-present account, or removing an absent one).
        changed: bool,
    },
    QueryError {
        message: String,
    },
}

/// Validates that `account` looks like a Hedera account ID, the same shape
/// check (`AccountId::from_str`) already used to validate account strings
/// elsewhere in ARIA (e.g. `build_x402_vault`, `payments/direct.rs`). The
/// `aria allowlist add/remove` CLI commands don't apply any validation today,
/// but the TCP endpoint accepts input over the network, so malformed input
/// is rejected here instead of silently corrupting the allowlist.
fn validate_account_format(account: &str) -> Result<(), String> {
    use std::str::FromStr;

    use hiero_sdk::AccountId;

    if account.trim().is_empty() {
        return Err("account must not be empty".to_string());
    }
    AccountId::from_str(account)
        .map(|_| ())
        .map_err(|e| format!("invalid account '{}': {}", account, e))
}

/// Single code path for mutating the payment allowlist, shared by the
/// `aria allowlist add/remove` CLI commands and the TCP `mutate_allowlist`
/// endpoint. Both callers go through this function so behavior can never
/// diverge between them; it calls the exact same `Db` methods the CLI
/// always has.
fn mutate_allowlist_entry(
    db: &Db,
    agent_did: &str,
    action: &str,
    account: &str,
) -> anyhow::Result<bool> {
    match action {
        "add" => {
            db.add_allowlist_entry(agent_did, account)?;
            Ok(true)
        }
        "remove" => db.remove_allowlist_entry(agent_did, account),
        other => anyhow::bail!("unknown allowlist action: {} (expected \"add\" or \"remove\")", other),
    }
}

/// Handles the TCP `mutate_allowlist` endpoint: validates the account
/// format, then calls `mutate_allowlist_entry` — the same function the CLI
/// uses — rather than reimplementing allowlist mutation here.
fn handle_mutate_allowlist(
    action: Option<&str>,
    account: Option<&str>,
    agent_did: &str,
    db: &Db,
) -> QueryResponse {
    let Some(action) = action else {
        return QueryResponse::QueryError {
            message: "missing \"action\" (expected \"add\" or \"remove\")".to_string(),
        };
    };
    let Some(account) = account else {
        return QueryResponse::QueryError { message: "missing \"account\"".to_string() };
    };
    if action != "add" && action != "remove" {
        return QueryResponse::QueryError {
            message: format!("unknown allowlist action: {} (expected \"add\" or \"remove\")", action),
        };
    }
    if let Err(msg) = validate_account_format(account) {
        return QueryResponse::QueryError { message: msg };
    }

    match mutate_allowlist_entry(db, agent_did, action, account) {
        Ok(changed) => QueryResponse::MutateAllowlist {
            agent_did: agent_did.to_string(),
            action: action.to_string(),
            account: account.to_string(),
            changed,
        },
        Err(e) => QueryResponse::QueryError { message: format!("allowlist mutation failed: {}", e) },
    }
}

/// Handles the four read-only TCP query endpoints. Never touches
/// `react_loop.rs` and never creates a task — answers straight from existing
/// state (db + governance config), or, for the wallet balance, a live
/// Hedera network read.
async fn handle_query(
    query: &str,
    agent_did: &str,
    db: &Db,
    runtime_cfg: &RuntimeConfig,
    payment_vault: Option<&crate::payments::direct::PaymentVault>,
    x402_vault: Option<&crate::payments::x402_vault::X402PaymentVault>,
) -> QueryResponse {
    match query {
        "query_budget" => {
            let governance = &runtime_cfg.governance;
            // Same two queries try_reserve_spend already uses — reused
            // directly rather than re-deriving the SQL here.
            let committed_spend_24h = db.get_daily_committed_spend(agent_did).unwrap_or(0.0);
            let held_spend = db.get_daily_held_spend(agent_did).unwrap_or(0.0);
            let remaining_budget =
                governance.per_day_cap.map(|cap| cap - committed_spend_24h - held_spend);

            QueryResponse::QueryBudget {
                agent_did: agent_did.to_string(),
                per_task_cap: governance.per_task_cap,
                per_day_cap: governance.per_day_cap,
                committed_spend_24h,
                held_spend,
                remaining_budget,
            }
        }
        "query_holds" => match db.list_payment_holds(agent_did) {
            Ok(holds) => QueryResponse::QueryHolds { agent_did: agent_did.to_string(), holds },
            Err(e) => QueryResponse::QueryError { message: format!("failed to load holds: {}", e) },
        },
        "query_allowlist" => match db.list_allowlist(agent_did) {
            Ok(accounts) => {
                QueryResponse::QueryAllowlist { agent_did: agent_did.to_string(), accounts }
            }
            Err(e) => {
                QueryResponse::QueryError { message: format!("failed to load allowlist: {}", e) }
            }
        },
        "query_wallet_balance" => {
            use hiero_sdk::AccountBalanceQuery;

            let (client, account_id) = if let Some(pv) = payment_vault {
                (pv.client(), pv.account_id())
            } else if let Some(xv) = x402_vault {
                (xv.client(), xv.account_id())
            } else {
                return QueryResponse::QueryError {
                    message: "no payment vault configured (HEDERA_ACCOUNT_ID / HEDERA_PRIVATE_KEY unset)"
                        .to_string(),
                };
            };

            // Live read against Hedera on every call — intentionally not
            // cached here.
            match AccountBalanceQuery::new().account_id(account_id).execute(&client).await {
                Ok(balance) => QueryResponse::QueryWalletBalance {
                    agent_did: agent_did.to_string(),
                    account_id: account_id.to_string(),
                    balance_hbar: balance.hbars.to_tinybars() as f64 / 100_000_000.0,
                },
                Err(e) => {
                    QueryResponse::QueryError { message: format!("balance query failed: {}", e) }
                }
            }
        }
        other => QueryResponse::QueryError { message: format!("unknown query type: {}", other) },
    }
}

fn print_help() {
    println!("ARIA — Governed Agent Runtime v0.6");
    println!();
    println!("Usage: aria [COMMAND]");
    println!();
    println!("Commands:");
    println!("  daemon                   Run headless TCP service (default)");
    println!("  allowlist add <account>   Add an account to the payment allowlist");
    println!("  allowlist remove <account> Remove an account from the payment allowlist");
    println!("  allowlist list           List allowlisted payment recipients");
    println!("  install                  Install systemd user service for auto-start");
    println!("  help                     Show this help");
    println!();
    println!("Examples:");
    println!("  aria");
    println!("  aria allowlist add 0.0.12345");
    println!("  aria allowlist list");
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
    let payment_vault: Option<Arc<crate::payments::direct::PaymentVault>> =
        crate::payments::direct::PaymentVault::try_from_env().map(Arc::new);

    let x402_vault: Option<Arc<crate::payments::x402_vault::X402PaymentVault>> =
        build_x402_vault(db.clone());

    let skills = Arc::new(SkillManager::new()?);

    let (did, pub_key) =
        db.get_identity()?.ok_or_else(|| anyhow::anyhow!("Identity missing from DB"))?;
    let (vault, level) = crate::identity::initialize_vault(did, pub_key).await?;
    let vault = Arc::new(vault);
    info!("Identity HAL initialized (Mode: {:?})", level);

    // Startup hold reconciliation: delete any holds for which a matching SUCCESS
    // payment already exists (leftover from a crash between payment success and
    // hold release — they would otherwise permanently squat on daily budget).
    match db.reconcile_stale_holds() {
        Ok(0) => {}
        Ok(n) => info!("Startup: reconciled {} stale payment hold(s) that matched SUCCESS payments.", n),
        Err(e) => tracing::warn!("Startup: hold reconciliation failed (non-fatal): {}", e),
    }

    if runtime_cfg.governance.audit_topic_id.is_none() {
        if let Some(ref pv) = payment_vault {
            let client = pv.client();
            if let Ok(tid) = crate::payments::audit::create_audit_topic(&client, "curb-audit").await {
                info!("Provisioned new HCS payment audit topic: {}", tid);
                println!("HCS Payment Audit Topic ID: {}", tid);
                let _ = db.set_config("hedera_payment_audit_topic", &tid);
            }
        } else if let Some(ref xv) = x402_vault {
            let client = xv.client();
            if let Ok(tid) = crate::payments::audit::create_audit_topic(&client, "curb-audit").await {
                info!("Provisioned new HCS payment audit topic: {}", tid);
                println!("HCS Payment Audit Topic ID: {}", tid);
                let _ = db.set_config("hedera_payment_audit_topic", &tid);
            }
        }
    }

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

                    if let Some(mutate_kind) = req.mutate.as_deref() {
                        let agent_did = vault.did();
                        let response = match mutate_kind {
                            "mutate_allowlist" => handle_mutate_allowlist(
                                req.action.as_deref(),
                                req.account.as_deref(),
                                &agent_did,
                                &db,
                            ),
                            other => QueryResponse::QueryError {
                                message: format!("unknown mutate type: {}", other),
                            },
                        };
                        let json = serde_json::to_string(&response).unwrap_or_default();
                        let _ = socket.write_all(format!("{}\n", json).as_bytes()).await;
                        return;
                    }

                    if let Some(query_kind) = req.query.as_deref() {
                        let agent_did = vault.did();
                        let response = handle_query(
                            query_kind,
                            &agent_did,
                            &db,
                            &runtime_cfg,
                            payment_vault.as_deref(),
                            x402_vault.as_deref(),
                        )
                        .await;
                        let json = serde_json::to_string(&response).unwrap_or_default();
                        let _ = socket.write_all(format!("{}\n", json).as_bytes()).await;
                        return;
                    }

                    info!("Received task: {}", req.task);

                    // Resume a paused task if a task_id was given and it's actually
                    // awaiting confirmation; otherwise fall back to a fresh task,
                    // same as before this change.
                    let mut resume: Option<(
                        String,
                        Vec<serde_json::Value>,
                        Option<serde_json::Value>,
                    )> = None;
                    if let Some(ref existing_id) = req.task_id {
                        if let Ok(Some(session)) = db.get_task_session(existing_id)
                            && session.status == "awaiting_confirmation"
                        {
                            let history: Vec<serde_json::Value> = session
                                .history_json
                                .as_deref()
                                .and_then(|h| serde_json::from_str(h).ok())
                                .unwrap_or_default();

                            let pending_action: Option<serde_json::Value> = session
                                .pending_action_json
                                .as_deref()
                                .and_then(|p| serde_json::from_str(p).ok());

                            resume = Some((existing_id.clone(), history, pending_action));
                        }
                    }

                    let (task_id, history, pending_action) = if let Some(r) = resume {
                        r
                    } else {
                        let new_task_id = db.new_task_id();

                        // FIX 1: Pass the actual `new_task_id` to fetch correct link info
                        let (link_hash, created_at) = match db.get_task_link_info(&new_task_id, &req.task) {
                            Ok(info) => info,
                            Err(_) => ("".to_string(), "".to_string()),
                        };

                        let task_chain_sig = vault.sign(link_hash.as_bytes()).await.unwrap_or_default();

                        let id = match db.create_task(
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
                        (
                            id,
                            vec![serde_json::json!({
                                "role": "user",
                                "content": req.task.clone()
                            })],
                            None,
                        )
                    };

                    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

                    let db_for_loop = db.clone();
                    let did_for_loop = vault.did();
                    let task_id_for_loop = task_id.clone();

                    let handle = tokio::spawn(async move {
                        crate::agent::react_loop::run_react_loop(
                            api_key, history, runtime_cfg.injected_config, skills, tx, req.task, req.skills_type,
                            db_for_loop, payment_vault, x402_vault, did_for_loop, task_id_for_loop, pending_action
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

                    match db.get_task_session(&task_id) {
                        Ok(Some(session))
                            if session.status == "awaiting_confirmation" =>
                        {
                            info!(
                                "Task {} is awaiting confirmation; leaving it unsealed for resume",
                                task_id
                            );
                        }
                        Ok(_) => {
                            if let Err(e) = db.seal_task(&task_id, status) {
                                warn!("Failed to seal task {}: {}", task_id, e);
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to inspect task {} before sealing: {}",
                                task_id, e
                            );
                            if let Err(e) = db.seal_task(&task_id, status) {
                                warn!("Failed to seal task {}: {}", task_id, e);
                            }
                        }
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
        "allowlist" => {
            let subcmd = args.get(2).map(|s| s.as_str()).unwrap_or("list");
            let db = bootstrap_db()?;
            let (agent_did, _) =
                db.get_identity()?.unwrap_or(("did:aria:jayesh".into(), String::new()));
            match subcmd {
                "add" => {
                    let account = args.get(3).ok_or_else(|| {
                        anyhow::anyhow!("Account required: aria allowlist add <account>")
                    })?;
                    mutate_allowlist_entry(&db, &agent_did, "add", account)?;
                    println!("✓ Account '{}' added to allowlist for {}", account, agent_did);
                }
                "remove" => {
                    let account = args.get(3).ok_or_else(|| {
                        anyhow::anyhow!("Account required: aria allowlist remove <account>")
                    })?;
                    let removed = mutate_allowlist_entry(&db, &agent_did, "remove", account)?;
                    if removed {
                        println!("✓ Account '{}' removed from allowlist for {}", account, agent_did);
                    } else {
                        println!("Account '{}' was not on allowlist for {}", account, agent_did);
                    }
                }
                "list" => {
                    let list = db.list_allowlist(&agent_did)?;
                    println!("Allowlisted accounts for {}:", agent_did);
                    if list.is_empty() {
                        println!("  (none)");
                    } else {
                        for acc in list {
                            println!("  • {}", acc);
                        }
                    }
                }
                _ => {
                    println!("Usage: aria allowlist <add|remove|list> [account]");
                }
            }
            Ok(())
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
