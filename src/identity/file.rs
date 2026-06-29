use async_trait::async_trait;
use crate::crypto;
use super::IdentityVault;

pub struct FileVault {
    did: String,
    public_key: String,
}

impl FileVault {
    pub fn new(did: String, public_key: String) -> Self {
        Self { did, public_key }
    }
}

#[async_trait]
impl IdentityVault for FileVault {
    fn did(&self) -> String {
        self.did.clone()
    }

    fn public_key(&self) -> String {
        self.public_key.clone()
    }

    fn security_level(&self) -> super::SecurityLevel {
        super::SecurityLevel::Software
    }

    async fn sign(&self, data: &[u8]) -> anyhow::Result<String> {
        let signing_key = crypto::load_signing_key(&self.did)?;
        Ok(crypto::sign_bytes(&signing_key, data))
    }
}
