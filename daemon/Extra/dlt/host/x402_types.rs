use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hiero_sdk::{
    AccountId,
    Client,
    Hbar,
    PrivateKey,
    TokenId,
    TransactionId,
    TransferTransaction,
};
use serde::{
    Deserialize,
    Serialize,
};
use thiserror::Error;

/// Matches x402 v2 exact-scheme on Hedera.
/// Represents the requirements for a payment request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PaymentRequirements {
    pub scheme: String,
    pub network: String,
    pub amount: String,
    pub asset: String,
    #[serde(rename = "payTo")]
    pub pay_to: String,
    #[serde(rename = "maxTimeoutSeconds")]
    pub max_timeout_seconds: u64,
    pub extra: serde_json::Value,
}

/// Matches x402 v2 exact-scheme on Hedera.
/// Represents the resource associated with the payment request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PaymentResource {
    pub url: String,
    pub description: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

/// Matches x402 v2 exact-scheme on Hedera.
/// Represents the envelope payload containing the serialized transaction transaction.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PaymentPayloadData {
    pub transaction: String,
}

/// Matches x402 v2 exact-scheme on Hedera.
/// Represents the payment payload envelope fulfilling a payment request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PaymentPayload {
    #[serde(rename = "x402Version")]
    pub x402_version: u32,
    pub resource: PaymentResource,
    pub accepted: PaymentRequirements,
    pub payload: PaymentPayloadData,
}

/// Matches x402 v2 exact-scheme on Hedera.
/// Represents the structure of a facilitator's response to a settle request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SettleResponse {
    pub success: bool,
    #[serde(rename = "errorReason")]
    pub error_reason: Option<String>,
    pub payer: Option<String>,
    #[serde(alias = "transactionId", alias = "transaction")]
    pub transaction_id: String,
    pub network: String,
}

/// Matches x402 v2 exact-scheme on Hedera.
/// Represents the structure of a facilitator's response to a verify request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VerifyResponse {
    #[serde(rename = "isValid")]
    pub is_valid: bool,
    #[serde(rename = "invalidReason")]
    pub invalid_reason: Option<String>,
    pub payer: Option<String>,
}

#[derive(Error, Debug)]
pub enum PaymentError {
    #[error("Missing fee payer")]
    MissingFeePayer,
    #[error("Invalid amount")]
    InvalidAmount,
    #[error("Hedera SDK error: {0}")]
    HederaSdkError(String),
    #[error("Facilitator error: {0}")]
    FacilitatorError(#[from] crate::payments::facilitator_client::FacilitatorError),
    #[error("Verification failed: {0}")]
    VerificationFailed(String),
}

/// Matches x402 v2 exact-scheme on Hedera.
/// Constructs and partially signs a Hedera TransferTransaction for an x402 payment.
pub fn build_payment_transaction(
    client_account_id: &AccountId,
    client_private_key: &PrivateKey,
    requirements: &PaymentRequirements,
    _hedera_client: &Client,
) -> Result<(String, String), PaymentError> {
    let amount: i64 = requirements.amount.parse().map_err(|_| PaymentError::InvalidAmount)?;
    if amount <= 0 {
        return Err(PaymentError::InvalidAmount);
    }

    let pay_to = AccountId::from_str(&requirements.pay_to)
        .map_err(|e| PaymentError::HederaSdkError(e.to_string()))?;

    let mut tx = TransferTransaction::new();

    if requirements.asset == "0.0.0" {
        tx.hbar_transfer(*client_account_id, Hbar::from_tinybars(-amount));
        tx.hbar_transfer(pay_to, Hbar::from_tinybars(amount));
    } else {
        let token_id = TokenId::from_str(&requirements.asset)
            .map_err(|e| PaymentError::HederaSdkError(e.to_string()))?;
        tx.token_transfer(token_id, *client_account_id, -amount);
        tx.token_transfer(token_id, pay_to, amount);
    }

    let fee_payer_val = requirements.extra.get("feePayer").ok_or(PaymentError::MissingFeePayer)?;
    let fee_payer_str = fee_payer_val.as_str().ok_or(PaymentError::MissingFeePayer)?;
    let fee_payer = AccountId::from_str(fee_payer_str)
        .map_err(|e| PaymentError::HederaSdkError(e.to_string()))?;

    let generated_tx_id = TransactionId::generate(fee_payer);
    tx.transaction_id(generated_tx_id);

    // Pin to a single node to ensure we don't generate a multi-node transaction list.
    // The facilitator JS SDK only supports decoding a 1-element list.
    // Reads from HEDERA_NODE_ACCOUNT_ID environment variable to avoid a hardcoded single point of failure.
    let node_account_str =
        std::env::var("HEDERA_NODE_ACCOUNT_ID").unwrap_or_else(|_| "0.0.3".to_string());
    let node_id = AccountId::from_str(&node_account_str).map_err(|e| {
        PaymentError::HederaSdkError(format!(
            "Invalid node account ID '{}': {}",
            node_account_str, e
        ))
    })?;
    tx.node_account_ids([node_id]);

    tx.freeze_with(None).map_err(|e| PaymentError::HederaSdkError(e.to_string()))?;

    tx.sign(client_private_key.clone());

    let signed_bytes = tx
        .to_signed_transaction_bytes()
        .map_err(|e| PaymentError::HederaSdkError(e.to_string()))?;

    Ok((STANDARD.encode(encode_as_transaction_list(signed_bytes)), generated_tx_id.to_string()))
}

/// Wraps raw `SignedTransaction` protobuf bytes into the single-entry
/// `TransactionList` envelope that the Hedera JS SDK's `Transaction.fromBytes()`
/// expects.  Encoded manually to avoid a direct `hiero-sdk-proto` dependency.
///
/// Wire layout:
///   TransactionList { transaction_list: [ Transaction { signed_transaction_bytes } ] }
///
///   TransactionList.transaction_list  = field 1, wire-type 2  → tag 0x0A
///   Transaction.signed_transaction_bytes = field 5, wire-type 2  → tag 0x2A
fn encode_as_transaction_list(signed_tx_bytes: Vec<u8>) -> Vec<u8> {
    // Inner: Transaction { signed_transaction_bytes: <signed_tx_bytes> }
    let tx_bytes = proto_field(0x2A, &signed_tx_bytes);
    // Outer: TransactionList { transaction_list: [<tx_bytes>] }
    proto_field(0x0A, &tx_bytes)
}

/// Serialises a single protobuf length-delimited field: `tag || varint(len) || data`.
fn proto_field(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + data.len());
    out.push(tag);
    // LEB-128 varint for the payload length
    let mut n = data.len();
    loop {
        let mut b = (n & 0x7F) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_build_payment_transaction_hbar() {
        let client_account_id = AccountId::from_str("0.0.1111").unwrap();
        let client_private_key = PrivateKey::generate_ed25519();

        let mut extra = serde_json::Map::new();
        extra.insert("feePayer".to_string(), serde_json::Value::String("0.0.5678".to_string()));

        let requirements = PaymentRequirements {
            scheme: "exact".to_string(),
            network: "hedera:testnet".to_string(),
            amount: "1000".to_string(),
            asset: "0.0.0".to_string(),
            pay_to: "0.0.1234".to_string(),
            max_timeout_seconds: 120,
            extra: serde_json::Value::Object(extra),
        };

        let hedera_client = Client::for_testnet();

        let res = build_payment_transaction(
            &client_account_id,
            &client_private_key,
            &requirements,
            &hedera_client,
        );

        assert!(res.is_ok(), "Expected Ok transaction, got: {:?}", res);
        let (tx_b64, tx_id) = res.unwrap();
        assert!(!tx_b64.is_empty(), "Transaction base64 should not be empty");
        assert!(!tx_id.is_empty(), "Transaction ID should not be empty");
    }

    #[tokio::test]
    async fn test_build_payment_transaction_ecdsa() {
        let client_account_id = AccountId::from_str("0.0.1111").unwrap();
        // Generate an ECDSA key to test the ECDSA path (regression test for ECDSA specific bugs)
        let client_private_key = PrivateKey::generate_ecdsa();

        let mut extra = serde_json::Map::new();
        extra.insert("feePayer".to_string(), serde_json::Value::String("0.0.5678".to_string()));

        let requirements = PaymentRequirements {
            scheme: "exact".to_string(),
            network: "hedera:testnet".to_string(),
            amount: "1000".to_string(),
            asset: "0.0.0".to_string(),
            pay_to: "0.0.1234".to_string(),
            max_timeout_seconds: 120,
            extra: serde_json::Value::Object(extra),
        };

        let hedera_client = Client::for_testnet();

        let res = build_payment_transaction(
            &client_account_id,
            &client_private_key,
            &requirements,
            &hedera_client,
        );

        assert!(res.is_ok(), "Expected Ok transaction, got: {:?}", res);
        let (tx_b64, tx_id) = res.unwrap();
        assert!(!tx_b64.is_empty(), "Transaction base64 should not be empty");
        assert!(!tx_id.is_empty(), "Transaction ID should not be empty");

        // BUG-2 regression coverage: explicitly decode the bytes and verify.
        let raw = base64::engine::general_purpose::STANDARD.decode(&tx_b64).unwrap();
        let _tx = hiero_sdk::AnyTransaction::from_bytes(&raw)
            .expect("Should decode back to AnyTransaction");
    }
}
