use hiero_sdk::{
    Client,
    TopicCreateTransaction,
    TopicId,
    TopicMessageSubmitTransaction,
};
use tracing::error;

#[derive(Debug, Clone, serde::Serialize)]
pub struct AriaRecord {
    pub v: u32,
    pub agent: String,
    pub ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "requestId")]
    pub request_id: Option<String>,
}

/// Create an immutable HCS audit topic with no admin key.
pub async fn create_audit_topic(client: &Client, memo: &str) -> anyhow::Result<String> {
    let mut tx = TopicCreateTransaction::new();
    tx.topic_memo(memo);
    let response = tx.execute(client).await?;
    let receipt = response.get_receipt(client).await?;
    let topic_id = receipt
        .topic_id
        .ok_or_else(|| anyhow::anyhow!("Topic creation receipt missing topic_id"))?;
    Ok(topic_id.to_string())
}

/// Non-blocking fire-and-forget write of an AriaRecord to Hedera Consensus Service.
/// An HCS failure or network error NEVER blocks, delays, or reverses the policy decision.
pub fn write_payment_decision(
    client: Option<Client>,
    topic_id_str: Option<String>,
    record: AriaRecord,
) {
    let (Some(client), Some(topic_id_str)) = (client, topic_id_str) else {
        return;
    };

    if topic_id_str.trim().is_empty() {
        return;
    }

    tokio::spawn(async move {
        use std::str::FromStr;
        let topic_id = match TopicId::from_str(&topic_id_str) {
            Ok(tid) => tid,
            Err(e) => {
                error!("[aria audit] Invalid HCS topic ID '{}': {}", topic_id_str, e);
                return;
            }
        };

        let payload = match serde_json::to_string(&record) {
            Ok(json) => json,
            Err(e) => {
                error!("[aria audit] Failed to serialize record: {}", e);
                return;
            }
        };

        let mut tx = TopicMessageSubmitTransaction::new();
        tx.topic_id(topic_id).message(payload.into_bytes());

        if let Err(e) = tx.execute(&client).await {
            error!(
                "[aria audit] HCS write failed ({}/{}): {}",
                record.policy.as_deref().unwrap_or(""),
                record.reason.as_deref().unwrap_or(""),
                e
            );
        }
    });
}
