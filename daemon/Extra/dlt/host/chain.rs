//! Read-only on-chain queries against the public Hedera Mirror Node REST API.
//!
//! No keys, no signing, no egress beyond a plain HTTPS GET to the public
//! mirror node. Used by the `chain.query` skill via `host_chain_query` so
//! the agent can answer balance / token / topic / contract / transaction /
//! block questions. All URL construction lives in [`build_path`] (pure, unit
//! tested); [`query`] only fetches and parses.

use std::time::Duration;

/// GET `url` and return the parsed JSON body.
///
/// On HTTP 429 retries once after ~1s; on any other non-2xx returns an `Err`
/// carrying the status code and a truncated body prefix for debuggability.
pub async fn query(
    kind: &str,
    id: Option<&str>,
    opts: &serde_json::Value,
    default_account: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let url = build_path(kind, id, opts, default_account)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let resp = client.get(&url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts == 1 {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            continue;
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            anyhow::bail!("mirror node returned {} for {}: {}", status, url, snippet);
        }
        let body: serde_json::Value = resp.json().await?;
        return Ok(body);
    }
}

/// Build the full `{base}/api/v1/...` mirror-node URL (including query
/// string) for `kind`. Pure constructor — no I/O, so unit tests cover the
/// routing table without touching the network.
///
/// - `limit` comes from `opts["limit"]` (default 25, capped at 100).
/// - `order` comes from `opts["order"]` (default `"desc"`).
/// - Account-scoped kinds fall back to `default_account` when `id` is absent.
fn build_path(
    kind: &str,
    id: Option<&str>,
    opts: &serde_json::Value,
    default_account: Option<&str>,
) -> anyhow::Result<String> {
    let base = super::mirror::mirror_base_url();
    let limit = opts
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(25)
        .min(100);
    let order = opts
        .get("order")
        .and_then(|v| v.as_str())
        .unwrap_or("desc");
    let opt_str = |key: &str| {
        opts.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    let url = match kind {
        "account" => {
            let acct = id.or(default_account).ok_or_else(|| {
                anyhow::anyhow!("chain query 'account' requires an account id")
            })?;
            format!("{}/api/v1/accounts/{}", base, acct.trim())
        }
        "account_tokens" => {
            let acct = id.or(default_account).ok_or_else(|| {
                anyhow::anyhow!("chain query 'account_tokens' requires an account id")
            })?;
            format!("{}/api/v1/accounts/{}/tokens?limit={}", base, acct.trim(), limit)
        }
        "account_allowances" => {
            let acct = id.or(default_account).ok_or_else(|| {
                anyhow::anyhow!("chain query 'account_allowances' requires an account id")
            })?;
            format!(
                "{}/api/v1/accounts/{}/allowances/tokens?limit={}",
                base,
                acct.trim(),
                limit
            )
        }
        "account_nfts" => {
            let acct = id.or(default_account).ok_or_else(|| {
                anyhow::anyhow!("chain query 'account_nfts' requires an account id")
            })?;
            format!("{}/api/v1/accounts/{}/nfts?limit={}", base, acct.trim(), limit)
        }
        "balances" => {
            let acct = id.or(default_account).ok_or_else(|| {
                anyhow::anyhow!("chain query 'balances' requires an account id")
            })?;
            format!("{}/api/v1/balances?account.id={}", base, acct.trim())
        }
        "token" => {
            let tid = required_id(kind, id)?;
            format!("{}/api/v1/tokens/{}", base, tid)
        }
        "token_balances" => {
            let tid = required_id(kind, id)?;
            format!("{}/api/v1/tokens/{}/balances?limit={}", base, tid, limit)
        }
        "topic" => {
            let tid = required_id(kind, id)?;
            format!("{}/api/v1/topics/{}", base, tid)
        }
        "topic_messages" => {
            let tid = required_id(kind, id)?;
            format!(
                "{}/api/v1/topics/{}/messages?limit={}&order={}",
                base, tid, limit, order
            )
        }
        "contract" => {
            let cid = required_id(kind, id)?;
            format!("{}/api/v1/contracts/{}", base, cid)
        }
        "contract_results" => {
            let cid = required_id(kind, id)?;
            format!("{}/api/v1/contracts/{}/results?limit={}", base, cid, limit)
        }
        "transaction" => {
            let txid = required_id(kind, id)?;
            format!("{}/api/v1/transactions/{}", base, txid)
        }
        "transactions" => {
            let mut q = format!("limit={}&order={}", limit, order);
            // `id` doubles as an account filter when no explicit filter is set.
            let account = opt_str("account.id").or(opt_str("account_id")).or(id);
            if let Some(a) = account {
                q.push_str(&format!("&account.id={}", a.trim()));
            }
            if let Some(t) = opt_str("token.id").or(opt_str("token_id")) {
                q.push_str(&format!("&token.id={}", t));
            }
            if let Some(t) = opt_str("topic.id").or(opt_str("topic_id")) {
                q.push_str(&format!("&topic.id={}", t));
            }
            if let Some(t) = opt_str("type") {
                q.push_str(&format!("&type={}", t));
            }
            if let Some(r) = opt_str("result") {
                q.push_str(&format!("&result={}", r));
            }
            format!("{}/api/v1/transactions?{}", base, q)
        }
        "block" => {
            let bid = required_id(kind, id)?;
            format!("{}/api/v1/blocks/{}", base, bid)
        }
        "network_supply" => format!("{}/api/v1/network/supply", base),
        other => anyhow::bail!("unknown chain query kind '{}'", other),
    };
    Ok(url)
}

fn required_id<'a>(kind: &str, id: Option<&'a str>) -> anyhow::Result<&'a str> {
    match id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => Ok(v),
        None => Err(anyhow::anyhow!("chain query '{}' requires an id", kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn account_defaults_to_agent_id() {
        let url = build_path("account", None, &json!({}), Some("0.0.1234")).unwrap();
        assert!(url.ends_with("/api/v1/accounts/0.0.1234"), "{}", url);
    }

    #[test]
    fn topic_messages_limit_and_order() {
        let url = build_path(
            "topic_messages",
            Some("0.0.555"),
            &json!({"limit": 10, "order": "asc"}),
            None,
        )
        .unwrap();
        assert!(url.contains("/api/v1/topics/0.0.555/messages"), "{}", url);
        assert!(url.contains("limit=10"), "{}", url);
        assert!(url.contains("order=asc"), "{}", url);
    }

    #[test]
    fn transactions_with_filters() {
        let url = build_path(
            "transactions",
            None,
            &json!({"limit": 5, "account_id": "0.0.1234", "token_id": "0.0.5678", "type": "CRYPTOTRANSFER"}),
            None,
        )
        .unwrap();
        assert!(url.contains("/api/v1/transactions?"), "{}", url);
        assert!(url.contains("limit=5"), "{}", url);
        assert!(url.contains("account.id=0.0.1234"), "{}", url);
        assert!(url.contains("token.id=0.0.5678"), "{}", url);
        assert!(url.contains("type=CRYPTOTRANSFER"), "{}", url);
    }

    #[test]
    fn token_info_path() {
        let url = build_path("token", Some("0.0.456"), &json!({}), None).unwrap();
        assert!(url.ends_with("/api/v1/tokens/0.0.456"), "{}", url);
    }

    #[test]
    fn unknown_kind_errors() {
        let err = build_path("nope", None, &json!({}), None).unwrap_err();
        assert!(err.to_string().contains("unknown chain query kind"), "{}", err);
    }
}
