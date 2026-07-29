use hiero_sdk::{
    AccountId,
    Client,
    PrivateKey,
};
use tracing::warn;

use crate::payments::facilitator_client::{
    FacilitatorClient,
    find_hedera_testnet_fee_payer,
};
use crate::payments::x402_types::{
    PaymentError,
    PaymentPayload,
    PaymentPayloadData,
    PaymentRequirements,
    PaymentResource,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaymentResult {
    pub transaction_id: String,
    pub payer: String,
    pub network: String,
    pub hashscan_url: String,
    pub payment_token: String,
}

pub struct X402PaymentVault {
    client: Client,
    operator_id: AccountId,
    private_key: PrivateKey,
    db: std::sync::Arc<crate::db::Db>,
    facilitator_url: String,
}

impl X402PaymentVault {
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// The operator account this vault pays from / holds balance in.
    pub fn account_id(&self) -> AccountId {
        self.operator_id
    }

    pub fn new(
        client: Client,
        operator_id: AccountId,
        private_key: PrivateKey,
        db: std::sync::Arc<crate::db::Db>,
        facilitator_url: String,
    ) -> Self {
        Self { client, operator_id, private_key, db, facilitator_url }
    }

    pub async fn pay(
        &self,
        requirements: PaymentRequirements,
        skill_called: &str,
        task_id: Option<&str>,
        memo: Option<&str>,
    ) -> Result<PaymentResult, PaymentError> {
        // 1. Fetch /supported from the facilitator, extract the hedera:testnet feePayer
        let facilitator = FacilitatorClient::new(self.facilitator_url.clone());
        let supported = facilitator.get_supported().await?;
        let fee_payer = find_hedera_testnet_fee_payer(&supported)
            .ok_or_else(|| PaymentError::MissingFeePayer)?;

        // 2. Construct/fill PaymentRequirements
        let mut requirements = requirements;
        if let Some(obj) = requirements.extra.as_object_mut() {
            obj.insert("feePayer".to_string(), serde_json::Value::String(fee_payer.clone()));
        } else {
            let mut obj = serde_json::Map::new();
            obj.insert("feePayer".to_string(), serde_json::Value::String(fee_payer.clone()));
            requirements.extra = serde_json::Value::Object(obj);
        }

        // 3. Build and partially sign the transaction. The Hedera TransactionId is
        // generated client-side (payer@valid_start), so we know it before any
        // network call — no need to wait for settlement to have something to log.
        let (transaction, generated_tx_id) = crate::payments::x402_types::build_payment_transaction(
            &self.operator_id,
            &self.private_key,
            &requirements,
            &self.client,
        )?;

        // Extract metadata for PaymentResource from requirements.extra if present
        let url = requirements.extra.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let description = requirements
            .extra
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Hedera x402 Payment")
            .to_string();
        let mime_type =
            requirements.extra.get("mimeType").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let resource = PaymentResource { url, description, mime_type };

        // 4. Wrap it into the v2 PaymentPayload envelope
        let payload = PaymentPayload {
            x402_version: 2,
            resource,
            accepted: requirements.clone(),
            payload: PaymentPayloadData { transaction },
        };

        // NOTE: Do NOT call facilitator.verify() here.
        // The server-node calls /verify + /settle itself when it receives the
        // PAYMENT-SIGNATURE header (standard x402 flow). Pre-verifying here
        // consumes the transaction's verify slot and causes the server-node's
        // own /verify call to fail as a replay — producing "HTTP 402 after
        // payment" on the retry GET. The server-node is the authoritative
        // verifier and settler.

        // NOTE: we deliberately do NOT call facilitator.settle() here. Settlement
        // actually submits the transaction to Hedera and consumes it — it can only
        // happen once. The resource server calls /verify + /settle itself when it
        // receives this payload via the PAYMENT-SIGNATURE header (standard x402
        // flow), so settling here first would mean the transaction is already
        // consumed by the time the server tries to settle it, and that second
        // settle attempt fails. The real settlement confirmation is read back from
        // the PAYMENT-RESPONSE header on the resource server's success response.
        let transaction_id = generated_tx_id;
        let payer = self.operator_id.to_string();
        let network = requirements.network.clone();
        let hashscan_url = format!("https://hashscan.io/testnet/transaction/{}", transaction_id);

        // 7. Log the payment as PENDING — it becomes SUCCESS once the resource
        // server's PAYMENT-RESPONSE header confirms actual on-chain settlement.
        let agent_did = self
            .db
            .get_identity()
            .ok()
            .flatten()
            .map(|(did, _)| did)
            .unwrap_or_else(|| "unknown".to_string());

        let amount_parsed: f64 = requirements.amount.parse().unwrap_or(0.0);
        let amount_hbar = amount_parsed / 100_000_000.0;

        if let Err(e) = self.db.insert_payment(
            task_id,
            &agent_did,
            skill_called,
            &requirements.pay_to,
            amount_hbar,
            memo.unwrap_or(""),
            &transaction_id,
            &hashscan_url,
            "PENDING",
        ) {
            warn!("Failed to log payment to db: {}", e);
        }

        let payment_payload_json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => {
                return Err(PaymentError::VerificationFailed(format!(
                    "Failed to serialize payment payload: {}",
                    e
                )));
            }
        };
        let payment_token = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            payment_payload_json.as_bytes(),
        );

        Ok(PaymentResult { transaction_id, payer, network, hashscan_url, payment_token })
    }
}
