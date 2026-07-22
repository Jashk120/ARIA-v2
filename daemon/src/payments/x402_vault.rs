use hiero_sdk::{AccountId, Client, PrivateKey};
use crate::payments::x402_types::{PaymentRequirements, PaymentPayload, PaymentPayloadData, PaymentResource, PaymentError};
use crate::payments::facilitator_client::{FacilitatorClient, find_hedera_testnet_fee_payer};
use std::str::FromStr;
use tracing::warn;

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
    pub fn new(
        client: Client,
        operator_id: AccountId,
        private_key: PrivateKey,
        db: std::sync::Arc<crate::db::Db>,
        facilitator_url: String,
    ) -> Self {
        Self {
            client,
            operator_id,
            private_key,
            db,
            facilitator_url,
        }
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

        // 3. Build and partially sign the transaction
        let transaction = crate::payments::x402_types::build_payment_transaction(
            &self.operator_id,
            &self.private_key,
            &requirements,
            &self.client,
        )?;

        // Extract metadata for PaymentResource from requirements.extra if present
        let url = requirements.extra.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let description = requirements.extra.get("description").and_then(|v| v.as_str()).unwrap_or("Hedera x402 Payment").to_string();
        let mime_type = requirements.extra.get("mimeType").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let resource = PaymentResource {
            url,
            description,
            mime_type,
        };

        // 4. Wrap it into the v2 PaymentPayload envelope
        let payload = PaymentPayload {
            x402_version: 2,
            resource,
            accepted: requirements.clone(),
            payload: PaymentPayloadData {
                transaction,
            },
        };

        // 5. Call facilitator.verify() first
        let payload_json = serde_json::to_string(&payload).unwrap_or_default();
        let payload_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload_json.as_bytes());
       

        let verify_res = facilitator.verify(&payload, &requirements).await?;
        if !verify_res.is_valid {
            let reason = verify_res.invalid_reason.unwrap_or_else(|| "Verification failed".to_string());
            return Err(PaymentError::VerificationFailed(reason));
        }

        // 6. Call facilitator.settle()
        let settle_res = facilitator.settle(&payload, &requirements).await?;
        if !settle_res.success {
            let reason = settle_res.error_reason.unwrap_or_else(|| "Settle failed".to_string());
            return Err(PaymentError::VerificationFailed(reason));
        }

        let transaction_id = settle_res.transaction_id.clone();
        let payer = settle_res.payer.clone().unwrap_or_else(|| self.operator_id.to_string());
        let network = settle_res.network.clone();
        let hashscan_url = format!("https://hashscan.io/testnet/transaction/{}", transaction_id);

        // 7. Log the payment to the SQLite `payments` table
        let agent_did = self.db.get_identity()
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
            if settle_res.success { "SUCCESS" } else { "FAILED" },
        ) {
            warn!("Failed to log payment to db: {}", e);
        }

        let payment_payload_json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => return Err(PaymentError::VerificationFailed(format!("Failed to serialize payment payload: {}", e))),
        };
        let payment_token = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payment_payload_json.as_bytes());

        Ok(PaymentResult {
            transaction_id,
            payer,
            network,
            hashscan_url,
            payment_token,
        })
    }
}
