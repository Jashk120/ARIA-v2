//! test_x402_payment — standalone integration binary
//!
//! Exercises the full x402 v2 payment pipeline against the LIVE facilitator at
//! <https://x402.org/facilitator> on Hedera testnet.  No WASM, no skill layer,
//! no ReAct loop is involved — this is a pure mechanical proof that the payment
//! pipeline works end-to-end.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin test_x402_payment
//! ```
//!
//! # Requirements
//!
//! A `.env` file (or environment variables) containing:
//! - `HEDERA_ACCOUNT_ID`  — e.g. `0.0.8859309`
//! - `HEDERA_PRIVATE_KEY` — ECDSA private key for that account
//! - `HEDERA_NETWORK`     — optional; defaults to `"testnet"`

use std::str::FromStr;
use std::sync::Arc;

use base64::Engine as _;
use hiero_sdk::{AccountId, Client, PrivateKey};

use aria_daemon::db::Db;
use aria_daemon::payments::facilitator_client::{FacilitatorClient, find_hedera_testnet_fee_payer};
use aria_daemon::payments::x402_types::PaymentRequirements;
use aria_daemon::payments::x402_vault::X402PaymentVault;

// ── Test parameters ───────────────────────────────────────────────────────────

/// The live x402 facilitator endpoint.
const FACILITATOR_URL: &str = "https://x402.org/facilitator";

/// Pay TO the facilitator's feePayer account — it exists on testnet, is a
/// different account from the payer, so the net credit to payTo will be
/// exactly AMOUNT (no self-cancellation).  For a production payment you'd
/// use the resource server's payTo account from the 402 response instead.
const PAY_TO: &str = "0.0.9185802";

/// 1 HBAR in tinybars — tiny enough that even a mistake costs almost nothing.
const AMOUNT: &str = "100000000";

/// "0.0.0" means HBAR (not a fungible token).
const ASSET: &str = "0.0.0";

// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Load .env so the HEDERA_* vars are available even when run via `cargo run`.
    dotenvy::dotenv().ok();

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║        x402 Payment Pipeline — Live Integration Test         ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    if let Err(e) = run().await {
        // Walk the full error chain so nothing gets swallowed into a generic message.
        eprintln!();
        eprintln!("FAILURE ─────────────────────────────────────────────────────");
        eprintln!("  Error: {e}");
        let mut src: &dyn std::error::Error = e.as_ref();
        while let Some(cause) = src.source() {
            eprintln!("  Caused by: {cause}");
            src = cause;
        }
        eprintln!("─────────────────────────────────────────────────────────────");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    // ── Load credentials ──────────────────────────────────────────────────────
    let account_id_str = std::env::var("HEDERA_ACCOUNT_ID")
        .map_err(|_| anyhow::anyhow!("HEDERA_ACCOUNT_ID not set — add it to .env"))?;
    let private_key_str = std::env::var("HEDERA_PRIVATE_KEY")
        .map_err(|_| anyhow::anyhow!("HEDERA_PRIVATE_KEY not set — add it to .env"))?;
    let network = std::env::var("HEDERA_NETWORK").unwrap_or_else(|_| "testnet".to_string());

    // Show enough of the key to confirm it loaded, without leaking the secret.
    let key_preview = format!(
        "{}…{}",
        &private_key_str[..8.min(private_key_str.len())],
        &private_key_str[private_key_str.len().saturating_sub(4)..],
    );
    println!("[credentials]");
    println!("  account : {account_id_str}");
    println!("  network : {network}");
    println!("  key     : {key_preview}");
    println!();

    let operator_id = AccountId::from_str(&account_id_str)
        .map_err(|e| anyhow::anyhow!("Bad HEDERA_ACCOUNT_ID '{account_id_str}': {e}"))?;
    let private_key = PrivateKey::from_str_ecdsa(&private_key_str)
        .map_err(|e| anyhow::anyhow!("Bad HEDERA_PRIVATE_KEY: {e}"))?;

    let hedera_client = match network.as_str() {
        "mainnet"    => Client::for_mainnet(),
        "previewnet" => Client::for_previewnet(),
        _            => Client::for_testnet(),
    };
    hedera_client.set_operator(operator_id, private_key.clone());

    // ── Step 1 — GET /supported ───────────────────────────────────────────────
    println!("[step 1]  GET {FACILITATOR_URL}/supported");

    let facilitator_standalone = FacilitatorClient::new(FACILITATOR_URL.to_string());
    let supported = facilitator_standalone
        .get_supported()
        .await
        .map_err(|e| anyhow::anyhow!("FacilitatorClient::get_supported() failed: {e}"))?;

    println!("  ✓ received {} supported kind(s):", supported.kinds.len());
    for k in &supported.kinds {
        let extra_str = k.extra.as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string());
        println!("      scheme={} network={} extra={extra_str}", k.scheme, k.network);
    }

    // Confirm the facilitator speaks hedera:testnet.
    if !supported.kinds.iter().any(|k| k.network == "hedera:testnet") {
        anyhow::bail!(
            "Facilitator does not list a 'hedera:testnet' entry in /supported — \
             cannot proceed.  Full response: {supported:?}"
        );
    }
    println!("  ✓ hedera:testnet entry confirmed");

    // Extract the feePayer the facilitator wants to use.
    let fee_payer = find_hedera_testnet_fee_payer(&supported).ok_or_else(|| {
        anyhow::anyhow!(
            "hedera:testnet entry exists but has no 'feePayer' field in its 'extra' object"
        )
    })?;
    println!("  feePayer = {fee_payer}");
    println!();

    // ── Step 2 — Construct PaymentRequirements ────────────────────────────────
    println!("[step 2]  Constructing PaymentRequirements");

    // We embed feePayer here so the requirements printed below are complete.
    // X402PaymentVault::pay() will also re-fetch /supported and re-inject it,
    // which is fine — it will get the same value.
    let extra = serde_json::json!({ "feePayer": fee_payer });
    let requirements = PaymentRequirements {
        scheme:              "exact".to_string(),
        network:             "hedera:testnet".to_string(),
        amount:              AMOUNT.to_string(),
        asset:               ASSET.to_string(),
        pay_to:              PAY_TO.to_string(),
        max_timeout_seconds: 60,
        extra,
    };

    println!("  scheme             = {}", requirements.scheme);
    println!("  network            = {}", requirements.network);
    println!("  amount             = {} tinybars  ({:.8} HBAR)",
             requirements.amount,
             requirements.amount.parse::<f64>().unwrap_or(0.0) / 1e8);
    println!("  asset              = {}", requirements.asset);
    println!("  payTo              = {}", requirements.pay_to);
    println!("  maxTimeoutSeconds  = {}", requirements.max_timeout_seconds);
    println!("  extra.feePayer     = {fee_payer}");
    println!();

    // ── Probe — local round-trip ──────────────────────────────────────────────
    // Build the transaction bytes locally and try to decode them with our own
    // SDK.  If THIS fails → the bug lives in our serialization (not facilitator
    // compat).  If it passes → the bytes are structurally valid Rust-side but
    // the JS SDK at x402.org/facilitator is rejecting them, which is a
    // cross-SDK compat issue.
    println!("[probe]   Building tx + running Rust SDK round-trip …");
    {
        // Build a fresh client purely for the probe (same config as vault will use).
        let probe_client = Client::for_testnet();
        probe_client.set_operator(operator_id, private_key.clone());

        let b64 = aria_daemon::payments::x402_types::build_payment_transaction(
            &operator_id,
            &private_key,
            &requirements,
            &probe_client,
        )
        .map_err(|e| anyhow::anyhow!("build_payment_transaction failed in probe: {e}"))?;

        println!("  b64 length  = {} chars", b64.len());
        println!("  b64 prefix  = {}…", &b64[..40.min(b64.len())]);

        let raw = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .map_err(|e| anyhow::anyhow!("probe: base64 decode failed: {e}"))?;

        println!("  raw bytes   = {} bytes", raw.len());

        // Attempt: decode as TransactionList (what to_bytes() produces).
        // from_bytes is only available on AnyTransaction (Transaction<AnyTransactionData>).
        match hiero_sdk::AnyTransaction::from_bytes(&raw) {
            Ok(tx) => {
                let tx_id = tx.get_transaction_id();
                let node_ids: Option<Vec<_>> = tx.get_node_account_ids()
                    .map(|ids| ids.to_vec());
                println!("  ✓ Rust SDK from_bytes: OK");
                println!("    transaction_id   = {:?}", tx_id);
                println!("    node_account_ids = {:?}", node_ids);
            }
            Err(e) => {
                // BUG IS LOCAL — serialization is broken at source.
                // The facilitator is irrelevant; we need to fix build_payment_transaction.
                println!("  ✗ Rust SDK from_bytes FAILED: {e}");
                println!("    → Bug is in our serialization, not in facilitator compat.");
                anyhow::bail!(
                    "Local round-trip failed — serialization is broken before \
                     we even reach the facilitator: {e}"
                );
            }
        }
    }
    println!();

    // ── Step 3 — X402PaymentVault::pay() ─────────────────────────────────────
    // This internally:
    //   a) re-fetches /supported and injects feePayer
    //   b) builds + signs the Hedera TransferTransaction
    //   c) wraps it in a v2 PaymentPayload envelope
    //   d) calls /verify on the facilitator
    //   e) calls /settle on the facilitator
    //   f) logs the payment to the local SQLite DB
    println!("[step 3]  Calling X402PaymentVault::pay()");
    println!("  (builds tx → calls /verify → calls /settle)");

    // Open the daemon's own SQLite store so the payment gets logged normally.
    let vault_db = Db::new()
        .map_err(|e| anyhow::anyhow!("Failed to open daemon DB (~/.aria/daemon.db): {e}"))?;

    let vault = Arc::new(X402PaymentVault::new(
        hedera_client,
        operator_id,
        private_key,
        Arc::new(vault_db),
        FACILITATOR_URL.to_string(),
    ));

    let result = vault
        .pay(requirements, "test_x402_payment", None, None)
        .await
        .map_err(|e| anyhow::anyhow!("vault.pay() failed: {e}"))?;

    // ── Step 4 — Print full PaymentResult ────────────────────────────────────
    println!();
    println!("[step 4]  PaymentResult");
    println!("  transaction_id  = {}", result.transaction_id);
    println!("  payer           = {}", result.payer);
    println!("  network         = {}", result.network);
    println!("  hashscan_url    = {}", result.hashscan_url);
    // Print the first 40 chars of the base64 payment token — enough to confirm
    // it's populated without flooding the terminal.
    let token_preview = {
        let t = &result.payment_token;
        format!("{}…", &t[..40.min(t.len())])
    };
    println!("  payment_token   = {token_preview}");
    println!();

    // ── Step 5 — Final verdict ────────────────────────────────────────────────
    println!("════════════════════════════════════════════════════════════════");
    println!("SUCCESS — check https://hashscan.io/testnet/transaction/{}", result.transaction_id);
    println!("════════════════════════════════════════════════════════════════");

    Ok(())
}
