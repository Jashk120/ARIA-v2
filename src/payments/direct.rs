use std::str::FromStr;

use anyhow::Context;
use hiero_sdk::{
    AccountId,
    Client,
    Hbar,
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
}
