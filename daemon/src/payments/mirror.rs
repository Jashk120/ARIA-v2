//! Hedera Mirror Node client.
//!
//! Two jobs:
//! - Chain-verify a payment's status on demand (`fetch_transaction_result`),
//!   used by `query_payment_history` so history reflects what actually
//!   happened on-chain, not just the locally-cached `payments.status` column.
//! - Poll an in-flight transaction in the background until it reaches a
//!   final state (`poll_until_final`), used right after a payment skill
//!   submits so the daemon — not any client — is the one watching for
//!   settlement.

use std::time::Duration;

/// Base URL for the Hedera Mirror Node REST API, selected the same way
/// `payments/direct.rs` picks a network client — via `HEDERA_NETWORK`
/// (defaulting to testnet).
fn mirror_base_url() -> &'static str {
    match std::env::var("HEDERA_NETWORK").unwrap_or_else(|_| "testnet".to_string()).as_str() {
        "mainnet" => "https://mainnet-public.mirrornode.hedera.com",
        _ => "https://testnet.mirrornode.hedera.com",
    }
}

/// Converts a Hedera SDK transaction id string (`0.0.1234@1690000000.123456789`)
/// into the mirror node REST API's path form (`0.0.1234-1690000000-123456789`).
/// Returns `None` if `sdk_tx_id` doesn't look like a Hedera transaction id.
pub fn to_mirror_tx_id(sdk_tx_id: &str) -> Option<String> {
    let (account, ts) = sdk_tx_id.split_once('@')?;
    let (secs, nanos) = ts.split_once('.')?;
    Some(format!("{}-{}-{}", account, secs, nanos))
}

/// A single lookup against the mirror node for one transaction.
///
/// - `Ok(Some(result))` — the mirror node has indexed it; `result` is its
///   consensus result string (e.g. `"SUCCESS"`, `"INVALID_SIGNATURE"`).
/// - `Ok(None)` — not indexed yet (still propagating) or the id didn't parse;
///   callers should treat this as "not verifiable right now", not a failure.
/// - `Err` — a genuine network/parse failure talking to the mirror node.
pub async fn fetch_transaction_result(sdk_tx_id: &str) -> anyhow::Result<Option<String>> {
    let Some(mirror_id) = to_mirror_tx_id(sdk_tx_id) else {
        return Ok(None);
    };
    let url = format!("{}/api/v1/transactions/{}", mirror_base_url(), mirror_id);

    let resp = reqwest::get(&url).await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!("mirror node returned {} for {}", resp.status(), mirror_id);
    }

    let body: serde_json::Value = resp.json().await?;
    let result = body
        .get("transactions")
        .and_then(|t| t.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tx| tx.get("result"))
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());
    Ok(result)
}

/// Polls the mirror node for `sdk_tx_id` until it reaches a final state
/// (any indexed `result`) or `max_attempts` is exhausted, sleeping
/// `interval` between attempts. Returns `None` if the transaction never
/// showed up in time. Never called directly from a request-handling path —
/// callers `tokio::spawn` this so submission is never blocked on settlement.
pub async fn poll_until_final(
    sdk_tx_id: &str,
    max_attempts: usize,
    interval: Duration,
) -> Option<String> {
    for _ in 0..max_attempts {
        match fetch_transaction_result(sdk_tx_id).await {
            Ok(Some(result)) => return Some(result),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("mirror node poll failed for {}: {}", sdk_tx_id, e);
            }
        }
        tokio::time::sleep(interval).await;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_mirror_tx_id_converts_sdk_format() {
        assert_eq!(
            to_mirror_tx_id("0.0.1234@1690000000.123456789"),
            Some("0.0.1234-1690000000-123456789".to_string())
        );
    }

    #[test]
    fn test_to_mirror_tx_id_rejects_malformed_input() {
        assert_eq!(to_mirror_tx_id("not-a-tx-id"), None);
        assert_eq!(to_mirror_tx_id("0.0.1234@missing-dot"), None);
    }
}
