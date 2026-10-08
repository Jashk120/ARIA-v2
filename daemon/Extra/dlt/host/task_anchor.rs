//! Per-task HCS audit anchor (one message per sealed task, not per step).
//!
//! Wired in via `anchor_task_seal` in main.rs (all four seal sites).

use hiero_sdk::Client;
use tracing::error;

use super::audit::{
    AriaRecord,
    write_payment_decision,
};
use crate::db::Db;

/// Pure record constructor for the per-task seal anchor.
///
/// Chain hash + step count are carried in `reason` (`AriaRecord` is fixed —
/// additive use of existing `Option` fields only, no struct changes).
pub fn build_task_anchor_record(
    agent_did: &str,
    task_id: &str,
    chain_hash: &str,
    step_count: usize,
    status: &str,
) -> AriaRecord {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    AriaRecord {
        v: 1,
        agent: agent_did.to_string(),
        ts: now_ms,
        policy: Some("aria.task-anchor".to_string()),
        method: Some("task.seal".to_string()),
        amount: None,
        currency: None,
        counterparty: None,
        allowed: Some(true),
        reason: Some(format!("sealed:{status}:steps={step_count}:chain={chain_hash}")),
        request_id: Some(task_id.to_string()),
    }
}

/// Fire-and-forget publish of one HCS anchor message for a sealed task.
///
/// Call sites: `anchor_task_seal` in main.rs (all four seal sites) —
// one call per seal, after seal succeeds.
///
/// Design choice: takes the already-resolved `client` + `topic_id` as params
/// (mirroring `write_payment_decision`) instead of re-loading `RuntimeConfig`
/// internally. The follow-up reads them from the same source every other HCS
/// write uses (`RuntimeConfig::load(&db).governance.audit_topic_id`, backed by
/// the `hedera_payment_audit_topic` db key + `HEDERA_PAYMENT_AUDIT_TOPIC` env),
/// so the air-gap follow-up can gate on `dlt_enabled` at the call site before
/// invoking this. An HCS error NEVER blocks or fails the seal — failures are
/// `tracing::error`-logged only. Unconfigured/empty topic is a silent no-op.
pub fn publish_task_anchor(
    db: &Db,
    client: Option<Client>,
    topic_id: Option<String>,
    agent_did: &str,
    task_id: &str,
    status: &str,
) {
    let topic_id = topic_id.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if client.is_none() || topic_id.is_none() {
        return;
    }
    let chain_hash = match db.get_task_chain_hash(task_id) {
        Ok(h) => h,
        Err(e) => {
            error!("[aria task-anchor] failed to read chain hash for task {}: {}", task_id, e);
            return;
        }
    };
    let step_count = match db.count_task_steps(task_id) {
        Ok(n) => n,
        Err(e) => {
            error!("[aria task-anchor] failed to count steps for task {}: {}", task_id, e);
            return;
        }
    };
    let record = build_task_anchor_record(agent_did, task_id, &chain_hash, step_count, status);
    write_payment_decision(client, topic_id, record);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_task_anchor_record_fields_and_round_trip() {
        let agent = "did:aria:test-agent";
        let task_id = "task-123";
        let chain = "abc123chainhash";
        let record = build_task_anchor_record(agent, task_id, chain, 3, "done");

        assert_eq!(record.v, 1);
        assert_eq!(record.agent, agent);
        assert!(record.ts > 0);
        assert_eq!(record.policy.as_deref(), Some("aria.task-anchor"));
        assert_eq!(record.method.as_deref(), Some("task.seal"));
        assert_eq!(record.counterparty, None);
        assert_eq!(record.amount, None);
        assert_eq!(record.allowed, Some(true));
        let reason = record.reason.as_deref().unwrap();
        assert!(reason.starts_with("sealed:done:steps=3"), "reason: {reason}");
        assert!(reason.contains(chain), "reason must carry the chain hash: {reason}");
        assert_eq!(record.request_id.as_deref(), Some(task_id));

        let json = serde_json::to_string(&record).unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back["v"], 1);
        assert_eq!(back["agent"], agent);
        assert_eq!(back["policy"], "aria.task-anchor");
        assert_eq!(back["method"], "task.seal");
        assert_eq!(back["allowed"], true);
        assert_eq!(back["reason"], reason);
        assert_eq!(back["requestId"], task_id);
        assert!(back.get("counterparty").is_none(), "None fields must be skipped");
        assert!(back.get("amount").is_none(), "None fields must be skipped");

        let failed = build_task_anchor_record(agent, task_id, chain, 0, "failed");
        assert_eq!(failed.reason.as_deref(), Some("sealed:failed:steps=0:chain=abc123chainhash"));
    }
}
