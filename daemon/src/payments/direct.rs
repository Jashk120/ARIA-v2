use std::str::FromStr;

use anyhow::{
    Context,
    anyhow,
};
use hiero_sdk::{
    AccountId,
    Client,
    Hbar,
    PrivateKey,
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

    /// Non-fatal variant of `from_env`: returns `None` (with a log line) when
    /// Hedera credentials aren't configured, instead of erroring out. Use this
    /// at daemon startup so direct HBAR payments being unconfigured doesn't
    /// prevent the daemon from running (mirrors `build_x402_vault`).
    pub fn try_from_env() -> Option<Self> {
        match Self::from_env() {
            Ok(vault) => Some(vault),
            Err(e) => {
                tracing::warn!("Direct HBAR payment vault not configured, skipping: {}", e);
                None
            }
        }
    }
    pub fn from_env() -> anyhow::Result<Self> {
        let network = std::env::var("HEDERA_NETWORK").unwrap_or_else(|_| "testnet".to_string());
        let account_id_str =
            std::env::var("HEDERA_ACCOUNT_ID").context("HEDERA_ACCOUNT_ID not set in env")?;
        let private_key_str =
            std::env::var("HEDERA_PRIVATE_KEY").context("HEDERA_PRIVATE_KEY not set in env")?;

        if private_key_str.trim().is_empty() {
            return Err(anyhow!(
                "HEDERA_PRIVATE_KEY is empty — generate a testnet account at portal.hedera.com first"
            ));
        }

        let operator_id =
            AccountId::from_str(&account_id_str).context("Invalid HEDERA_ACCOUNT_ID")?;
        let private_key =
            PrivateKey::from_str_ecdsa(&private_key_str).context("Invalid HEDERA_PRIVATE_KEY")?;

        let client = match network.as_str() {
            "mainnet" => Client::for_mainnet(),
            "previewnet" => Client::for_previewnet(),
            _ => Client::for_testnet(),
        };
        client.set_operator(operator_id, private_key);
        Ok(Self { client, operator_id })
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
        let receipt = response.get_receipt(&self.client).await.context("get receipt failed")?;
        let tx_id = response.transaction_id.to_string();

        Ok(PaymentReceipt {
            hashscan_url: format!("https://hashscan.io/testnet/transaction/{}", tx_id),
            transaction_id: tx_id,
            status: format!("{:?}", receipt.status),
        })
    }
}
