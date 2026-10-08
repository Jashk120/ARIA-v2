use std::str::FromStr;

use anyhow::Context;
use hiero_sdk::{
    AccountId,
    Client,
    Hbar,
    TokenId,
    TransferTransaction,
};

pub struct PaymentReceipt {
    pub transaction_id: String,
    pub hashscan_url: String,
    pub status: String,
}

pub struct PaymentVault {
    client: Client,
    operator_id: AccountId,
}

impl PaymentVault {
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// The operator account this vault pays from / holds balance in.
    pub fn account_id(&self) -> AccountId {
        self.operator_id
    }

    /// Construct from the shared operator client, built once by the ARIA host.
    /// The host owns the Hedera client; this vault just holds a cheap
    /// `Arc<ClientInner>` clone plus the operator account it pays from.
    pub fn new(client: Client, operator_id: AccountId) -> Self {
        Self { client, operator_id }
    }
    pub async fn pay(
        &self,
        recipient: &str,
        amount_hbar: f64,
        memo: &str,
    ) -> anyhow::Result<PaymentReceipt> {
        let recipient_id = AccountId::from_str(recipient).context("invalid recipient AccountId")?;
        let amount = Hbar::from_tinybars((amount_hbar * 100_000_000.0) as i64);

        let mut tx = TransferTransaction::new();
        tx.hbar_transfer(self.operator_id, -amount)
            .hbar_transfer(recipient_id, amount)
            .transaction_memo(memo);

        let response = tx.execute(&self.client).await.context("submit transfer failed")?;
        // get_receipt() validates status by default (validate_status: true) and
        // returns Err for anything other than Status::Success — see
        // transaction_receipt_query.rs in hiero-sdk. So reaching this line means
        // the transfer is confirmed successful; we don't need (and shouldn't use)
        // the SDK's own Debug formatting of the status enum here.
        //
        // hiero-sdk-proto generates Rust enum variants from the protobuf
        // ResponseCodeEnum via prost-build's default PascalCase conversion, so
        // `format!("{:?}", receipt.status)` for a success renders as "Success",
        // not "SUCCESS". The `payments` table and every budget/cap SQL query in
        // db.rs filter on the literal string 'SUCCESS' (case-sensitive), so that
        // mismatch meant *every* direct HBAR payment was recorded under a status
        // that never matched — direct transfers never counted toward
        // committed_spend_24h or the per-day cap, no matter how many were made.
        let receipt = response.get_receipt(&self.client).await.context("get receipt failed")?;
        let tx_id = response.transaction_id.to_string();

        Ok(PaymentReceipt {
            hashscan_url: format!("https://hashscan.io/testnet/transaction/{}", tx_id),
            transaction_id: tx_id,
            status: "SUCCESS".to_string(),
        })
    }

    /// Send an HTS fungible token (`token_id`, e.g. a testnet USDC id) from
    /// the operator account to `recipient`. `amount` is in the token's base
    /// (lowest-denomination) units — callers convert from human units using
    /// [`Self::token_decimals`]. Otherwise identical to [`Self::pay`]:
    /// receipt-validated, same hashscan URL pattern, `"SUCCESS"` status so
    /// the `payments` table and budget/cap queries keep working unchanged.
    pub async fn pay_token(
        &self,
        token_id: &str,
        recipient: &str,
        amount: i64,
        memo: &str,
    ) -> anyhow::Result<PaymentReceipt> {
        let token_id = TokenId::from_str(token_id).context("invalid token TokenId")?;
        let recipient_id = AccountId::from_str(recipient).context("invalid recipient AccountId")?;

        let mut tx = TransferTransaction::new();
        tx.token_transfer(token_id, self.operator_id, -amount)
            .token_transfer(token_id, recipient_id, amount)
            .transaction_memo(memo);

        let response = tx.execute(&self.client).await.context("submit transfer failed")?;
        // Same receipt-validation contract as `pay`: reaching this line means
        // confirmed success; `status` is the literal 'SUCCESS' the db queries
        // filter on (see `pay` for why we don't Debug-format the SDK status).
        let receipt = response.get_receipt(&self.client).await.context("get receipt failed")?;
        let tx_id = response.transaction_id.to_string();

        Ok(PaymentReceipt {
            hashscan_url: format!("https://hashscan.io/testnet/transaction/{}", tx_id),
            transaction_id: tx_id,
            status: "SUCCESS".to_string(),
        })
    }

    /// Look up an HTS token's `decimals` via the mirror node REST API, using
    /// the same `HEDERA_NETWORK` → base-URL selection as `mirror.rs`.
    /// Returns `None` on any error (bad id, network failure, missing field)
    /// so callers can fall back to 0 decimals (base units as-is).
    pub async fn token_decimals(&self, token_id: &str) -> Option<u32> {
        let base = match std::env::var("HEDERA_NETWORK")
            .unwrap_or_else(|_| "testnet".to_string())
            .as_str()
        {
            "mainnet" => "https://mainnet-public.mirrornode.hedera.com",
            _ => "https://testnet.mirrornode.hedera.com",
        };
        let url = format!("{}/api/v1/tokens/{}", base, token_id);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .ok()?;
        let body: serde_json::Value = client.get(&url).send().await.ok()?.json().await.ok()?;
        let decimals = body.get("decimals")?;
        // Mirror node encodes `decimals` as a string ("6"); accept a raw
        // number too in case the encoding ever changes.
        if let Some(n) = decimals.as_u64() {
            return u32::try_from(n).ok();
        }
        decimals.as_str()?.parse::<u32>().ok()
    }
}
