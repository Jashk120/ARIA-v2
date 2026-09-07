use serde::Deserialize;
use thiserror::Error;

use super::x402_types::{
    PaymentPayload,
    PaymentRequirements,
    SettleResponse,
    VerifyResponse,
};

#[derive(Deserialize, Debug, Clone)]
pub struct SupportedKind {
    #[serde(rename = "x402Version")]
    pub x402_version: u32,
    pub scheme: String,
    pub network: String,
    pub extra: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SupportedKindsResponse {
    pub kinds: Vec<SupportedKind>,
}

#[derive(Error, Debug)]
pub enum FacilitatorError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Bad request from facilitator: {0}")]
    BadRequest(String),
    #[error("Unexpected HTTP status code: {0}")]
    UnexpectedStatus(u16),
}

pub struct FacilitatorClient {
    client: reqwest::Client,
    base_url: String,
}

impl FacilitatorClient {
    pub fn new(base_url: String) -> Self {
        Self { client: reqwest::Client::new(), base_url }
    }

    pub async fn get_supported(&self) -> Result<SupportedKindsResponse, FacilitatorError> {
        let url = format!("{}/supported", self.base_url.trim_end_matches('/'));
        let resp = self.client.get(&url).send().await?;

        if resp.status().is_success() {
            let res = resp.json::<SupportedKindsResponse>().await?;
            Ok(res)
        } else {
            Err(FacilitatorError::UnexpectedStatus(resp.status().as_u16()))
        }
    }

    pub async fn verify(
        &self,
        payload: &PaymentPayload,
        requirements: &PaymentRequirements,
    ) -> Result<VerifyResponse, FacilitatorError> {
        let url = format!("{}/verify", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "paymentPayload": payload,
            "paymentRequirements": requirements,
        });

        let resp = self.client.post(&url).json(&body).send().await?;
        let status = resp.status();

        if status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            eprintln!("DEBUG: Facilitator /verify SUCCESS raw body: {}", text);
            let res = serde_json::from_str::<VerifyResponse>(&text)
                .map_err(|e| FacilitatorError::BadRequest(format!("decode error: {}", e)))?;
            Ok(res)
        } else if status == reqwest::StatusCode::BAD_REQUEST {
            let error_text = resp.text().await.unwrap_or_default();
            eprintln!("DEBUG: Facilitator /verify BAD_REQUEST raw body: {}", error_text);
            #[derive(Deserialize)]
            struct ErrorBody {
                error: String,
            }
            if let Ok(err_body) = serde_json::from_str::<ErrorBody>(&error_text) {
                Err(FacilitatorError::BadRequest(err_body.error))
            } else {
                Err(FacilitatorError::BadRequest(error_text))
            }
        } else {
            let error_text = resp.text().await.unwrap_or_default();
            eprintln!("DEBUG: Facilitator /verify HTTP {} raw body: {}", status, error_text);
            Err(FacilitatorError::UnexpectedStatus(status.as_u16()))
        }
    }

    pub async fn settle(
        &self,
        payload: &PaymentPayload,
        requirements: &PaymentRequirements,
    ) -> Result<SettleResponse, FacilitatorError> {
        let url = format!("{}/settle", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "paymentPayload": payload,
            "paymentRequirements": requirements,
        });

        let resp = self.client.post(&url).json(&body).send().await?;
        let status = resp.status();

        if status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            eprintln!("DEBUG: Facilitator /settle SUCCESS raw body: {}", text);
            let res = serde_json::from_str::<SettleResponse>(&text)
                .map_err(|e| FacilitatorError::BadRequest(format!("decode error: {}", e)))?;
            Ok(res)
        } else if status == reqwest::StatusCode::BAD_REQUEST {
            let error_text = resp.text().await.unwrap_or_default();
            #[derive(Deserialize)]
            struct ErrorBody {
                error: String,
            }
            if let Ok(err_body) = serde_json::from_str::<ErrorBody>(&error_text) {
                Err(FacilitatorError::BadRequest(err_body.error))
            } else {
                Err(FacilitatorError::BadRequest(error_text))
            }
        } else {
            Err(FacilitatorError::UnexpectedStatus(status.as_u16()))
        }
    }
}

pub fn find_hedera_testnet_fee_payer(response: &SupportedKindsResponse) -> Option<String> {
    for kind in &response.kinds {
        if kind.network == "hedera:testnet"
            && let Some(extra) = &kind.extra
            && let Some(fee_payer) = extra.get("feePayer")
            && let Some(fee_payer_str) = fee_payer.as_str()
        {
            return Some(fee_payer_str.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn test_get_supported_integration() {
        let client = FacilitatorClient::new("https://x402.org/facilitator".to_string());
        let res = client.get_supported().await;
        assert!(res.is_ok(), "Expected Ok supported response, got: {:?}", res);

        let response = res.unwrap();
        let has_hedera_testnet = response.kinds.iter().any(|kind| kind.network == "hedera:testnet");
        assert!(has_hedera_testnet, "Expected kinds to contain hedera:testnet entry");
    }
}
