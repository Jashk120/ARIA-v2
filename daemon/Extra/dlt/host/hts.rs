use std::str::FromStr;

use anyhow::Context;
use hiero_sdk::{
    AccountAllowanceApproveTransaction,
    AccountId,
    Client,
    Hbar,
    PrivateKey,
    TokenCreateTransaction,
    TokenId,
    TokenMintTransaction,
    TokenSupplyType,
    TokenType,
};

pub struct TokenReceipt {
    pub transaction_id: String,
    pub hashscan_url: String,
    pub status: String,
}

fn receipt(transaction_id: String) -> TokenReceipt {
    TokenReceipt {
        hashscan_url: format!("https://hashscan.io/testnet/transaction/{}", transaction_id),
        transaction_id,
        status: "SUCCESS".to_string(),
    }
}

async fn confirmed(
    client: &Client,
    response: hiero_sdk::TransactionResponse,
) -> anyhow::Result<TokenReceipt> {
    let tx_id = response.transaction_id.to_string();
    let _ = response.get_receipt(client).await.context("get receipt failed")?;
    Ok(receipt(tx_id))
}

/// Create a fungible HTS token (RWA tokenization). Treasury is the operator
/// (auto-associated); admin + supply keys are the operator's public key so
/// the operator can mint later. Infinite supply type — the initial mint is
/// `initial_supply`, further supply comes from [`mint_fungible`].
/// Returns `(token_id, receipt)`.
pub async fn create_fungible_token(
    client: &Client,
    operator: AccountId,
    operator_key: &PrivateKey,
    name: &str,
    symbol: &str,
    decimals: u32,
    initial_supply: u64,
    treasury: AccountId,
    memo: &str,
) -> anyhow::Result<(String, TokenReceipt)> {
    let mut tx = TokenCreateTransaction::new();
    tx.name(name)
        .symbol(symbol)
        .decimals(decimals)
        .initial_supply(initial_supply)
        .treasury_account_id(treasury)
        .admin_key(operator_key.public_key())
        .supply_key(operator_key.public_key())
        .token_type(TokenType::FungibleCommon)
        .token_supply_type(TokenSupplyType::Infinite)
        .token_memo(memo);
    let _ = operator;
    let response = tx.execute(client).await.context("submit token create failed")?;
    let tx_id = response.transaction_id.to_string();
    let tx_receipt = response.get_receipt(client).await.context("get receipt failed")?;
    let token_id = tx_receipt
        .token_id
        .map(|id| id.to_string())
        .context("token create receipt has no token id")?;
    Ok((token_id, receipt(tx_id)))
}

/// Create an NFT collection (`TokenType::NonFungibleUnique`) with a finite
/// `max_supply`. Same operator-keys + treasury convention as
/// [`create_fungible_token`]. Returns `(token_id, receipt)`.
pub async fn create_nft(
    client: &Client,
    operator: AccountId,
    operator_key: &PrivateKey,
    name: &str,
    symbol: &str,
    max_supply: u64,
    treasury: AccountId,
    memo: &str,
) -> anyhow::Result<(String, TokenReceipt)> {
    let mut tx = TokenCreateTransaction::new();
    tx.name(name)
        .symbol(symbol)
        .decimals(0)
        .initial_supply(0)
        .treasury_account_id(treasury)
        .admin_key(operator_key.public_key())
        .supply_key(operator_key.public_key())
        .token_type(TokenType::NonFungibleUnique)
        .token_supply_type(TokenSupplyType::Finite)
        .max_supply(max_supply)
        .token_memo(memo);
    let _ = operator;
    let response = tx.execute(client).await.context("submit NFT create failed")?;
    let tx_id = response.transaction_id.to_string();
    let tx_receipt = response.get_receipt(client).await.context("get receipt failed")?;
    let token_id = tx_receipt
        .token_id
        .map(|id| id.to_string())
        .context("NFT create receipt has no token id")?;
    Ok((token_id, receipt(tx_id)))
}

/// Mint `amount` base units of a fungible token to the treasury. The client
/// operator holds the supply key, so no extra signing is needed.
pub async fn mint_fungible(
    client: &Client,
    token_id: &str,
    amount: u64,
) -> anyhow::Result<TokenReceipt> {
    let token_id = TokenId::from_str(token_id).context("invalid token TokenId")?;
    let mut tx = TokenMintTransaction::new();
    tx.token_id(token_id).amount(amount);
    let response = tx.execute(client).await.context("submit token mint failed")?;
    confirmed(client, response).await
}

/// Mint one NFT with `metadata` bytes to the treasury. Same operator-signs
/// convention as [`mint_fungible`].
pub async fn mint_nft(client: &Client, token_id: &str, metadata: Vec<u8>) -> anyhow::Result<TokenReceipt> {
    let token_id = TokenId::from_str(token_id).context("invalid token TokenId")?;
    let mut tx = TokenMintTransaction::new();
    tx.token_id(token_id).metadata([metadata]);
    let response = tx.execute(client).await.context("submit NFT mint failed")?;
    confirmed(client, response).await
}

/// Approve a HIP-336 fungible-token allowance: `spender` may move up to
/// `amount` base units of `token_id` from the operator account.
pub async fn approve_token_allowance(
    client: &Client,
    operator: AccountId,
    token_id: &str,
    spender: &str,
    amount: u64,
) -> anyhow::Result<TokenReceipt> {
    let token_id = TokenId::from_str(token_id).context("invalid token TokenId")?;
    let spender_id = AccountId::from_str(spender).context("invalid spender AccountId")?;
    let mut tx = AccountAllowanceApproveTransaction::new();
    tx.approve_token_allowance(token_id, operator, spender_id, amount);
    let response = tx.execute(client).await.context("submit token allowance failed")?;
    confirmed(client, response).await
}

/// Approve a HIP-336 HBAR allowance: `spender` may move up to
/// `amount_tinybars` from the operator account.
pub async fn approve_hbar_allowance(
    client: &Client,
    operator: AccountId,
    spender: &str,
    amount_tinybars: i64,
) -> anyhow::Result<TokenReceipt> {
    let spender_id = AccountId::from_str(spender).context("invalid spender AccountId")?;
    let mut tx = AccountAllowanceApproveTransaction::new();
    tx.approve_hbar_allowance(operator, spender_id, Hbar::from_tinybars(amount_tinybars));
    let response = tx.execute(client).await.context("submit HBAR allowance failed")?;
    confirmed(client, response).await
}
