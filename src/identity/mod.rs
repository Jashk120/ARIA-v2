use std::sync::Arc;
use async_trait::async_trait;
use tracing::{info, warn};

pub mod file;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityLevel {
    Hardware, // TPM 2.0 / Secure Enclave
    Software, // Encrypted File-based (current)
}

#[async_trait]
pub trait IdentityVault: Send + Sync {
    fn did(&self) -> String;
    fn public_key(&self) -> String;
    fn security_level(&self) -> SecurityLevel;
    async fn sign(&self, data: &[u8]) -> anyhow::Result<String>;
}

/// Probes the system for the best available security and returns the appropriate Vault.
pub async fn initialize_vault(did: String, public_key: String) -> anyhow::Result<(Arc<dyn IdentityVault>, SecurityLevel)> {
    // 1. Probe for Hardware TPM (Placeholder for Phase 3)
    info!("Probing for hardware security modules...");
    
    let tpm_available = false; // TODO: Implement actual hardware detection logic
    
    if tpm_available {
        info!("✓ TPM 2.0 detected. Initializing Hardware Identity Vault.");
        // return Ok((Arc::new(TpmVault::new(...)), SecurityLevel::Hardware));
    }

    // 2. Fallback to File-based Software Vault
    warn!("⚠ No hardware security module found. Falling back to encrypted Software Vault.");
    let vault = Arc::new(file::FileVault::new(did, public_key));
    Ok((vault, SecurityLevel::Software))
}
