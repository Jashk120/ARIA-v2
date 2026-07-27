//! Shared payment-governance helpers used by both the hedera_pay confirm/deny
//! flow (agent::react_loop) and the x402_pay autonomous path (skills::wasm_runtime).
//!
//! x402 payments never go through a human confirm/deny step, so the checks
//! that gate them (allowlist, per-task cap, per-day cap, rate limit) all run
//! inline in the same call that attempts the payment. hedera_pay runs the
//! same kind of checks but earlier, at proposal time, before the user is
//! asked to confirm.

/// Deterministic key identifying a (agent, recipient, amount) payment for
/// idempotent spend-hold tracking. The same recipient+amount combination
/// collapses to the same key so a retry (or a duplicate confirm) doesn't
/// double-reserve budget.
pub fn compute_payment_key(agent_did: &str, recipient: &str, amount_hbar: f64) -> String {
    let raw = format!("{}:{}:{}", agent_did, recipient, amount_hbar);
    let hash = crate::crypto::sha256_hex_str(&raw);
    hash[..16].to_string()
}
