//! ARIA Cryptographic Identity Layer — Phase 2
//!
//! Responsibilities:
//!   1. Ed25519 keypair generation for DIDs
//!   2. Private key encryption at rest (AES-256-GCM, Argon2id key derivation)
//!   3. Ed25519 signing / verification of audit log entries
//!   4. SHA-256 hashing for input/result/chain digests
//!   5. Multibase (base58btc) encode/decode for public keys in DID documents
//!
//! Key derivation for at-rest encryption:
//!   - The encryption key is derived from a device secret (a 32-byte random
//!     file at ~/.aria/device.key, generated once on first run).
//!   - Argon2id(device_secret || did_salt) → 32-byte AES key
//!   - Each encrypted blob is: nonce(12) || ciphertext || tag
//!
//! Audit chain:
//!   prev_hash = SHA-256(prev_hash || timestamp || skill_called || input_hash || result_hash)
//!   signature  = Ed25519(agent_signing_key, prev_hash_of_this_entry)
//!
//! The chain means you can't rewrite any entry without invalidating every
//! subsequent signature, even if you have the key for those later entries.

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, anyhow};
use argon2::{Argon2, Params, Version, Algorithm};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};
use zeroize::Zeroizing;

// ── Device secret ─────────────────────────────────────────────────────────────

/// Path to the 32-byte random device secret used as Argon2 input.
/// Created once; never transmitted; only meaningful on this machine.
fn device_key_path() -> anyhow::Result<PathBuf> {
    let mut p = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    p.push(".aria");
    p.push("device.key");
    Ok(p)
}

/// Read the device secret, generating it on first use.
pub fn load_or_create_device_secret() -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let path = device_key_path()?;
    if path.exists() {
        let bytes = fs::read(&path).context("reading device.key")?;
        if bytes.len() != 32 {
            anyhow::bail!("device.key is corrupt (wrong length)");
        }
        return Ok(Zeroizing::new(bytes));
    }

    // First run — generate and persist
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir)?;

    let mut secret = Zeroizing::new(vec![0u8; 32]);
    OsRng.fill_bytes(&mut secret);

    // 0600 permissions on Linux
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(&path, secret.as_slice())?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    fs::write(&path, secret.as_slice())?;

    Ok(secret)
}

// ── Key derivation ────────────────────────────────────────────────────────────

/// Derive a 32-byte AES key from device_secret + a per-identity salt (the DID string bytes).
/// Uses Argon2id with modest parameters — this only runs at key generation and unlock,
/// not per-request.
fn derive_aes_key(device_secret: &[u8], did_salt: &[u8]) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let params = Params::new(
        64 * 1024, // 64 MiB memory
        3,         // 3 iterations
        1,         // 1 thread (we're single-threaded here)
        Some(32),  // 32-byte output
    )
    .map_err(|e| anyhow!("argon2 params: {}", e))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(device_secret, did_salt, key.as_mut())
        .map_err(|e| anyhow!("argon2 hash: {}", e))?;
    Ok(key)
}

// ── Encryption / decryption ───────────────────────────────────────────────────

/// Encrypt `plaintext` with AES-256-GCM.
/// Returns `nonce(12 bytes) || ciphertext+tag` as a hex string.
pub fn encrypt_key_bytes(plaintext: &[u8], aes_key: &[u8; 32]) -> anyhow::Result<String> {
    let key = Key::<Aes256Gcm>::from_slice(aes_key);
    let cipher = Aes256Gcm::new(key);

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow!("AES-GCM encrypt: {}", e))?;

    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(hex::encode(out))
}

/// Decrypt a hex blob produced by `encrypt_key_bytes`.
pub fn decrypt_key_bytes(hex_blob: &str, aes_key: &[u8; 32]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let blob = hex::decode(hex_blob).context("hex decode")?;
    if blob.len() < 12 {
        anyhow::bail!("encrypted key blob too short");
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let key = Key::<Aes256Gcm>::from_slice(aes_key);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow!("AES-GCM decrypt failed — wrong device key or corrupt blob"))?;
    Ok(Zeroizing::new(plaintext))
}

// ── Identity ──────────────────────────────────────────────────────────────────

fn identity_key_path() -> anyhow::Result<PathBuf> {
    let mut p = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    p.push(".aria");
    p.push("id.key");
    Ok(p)
}

pub struct Identity {
    pub did: String,
    /// Multibase base58btc encoded (starts with 'z')
    pub public_key_multibase: String,
}

/// Generate a fresh Ed25519 keypair, encrypt the private key, and save it to `~/.aria/id.key`.
/// Returns the public parts as an `Identity`.
/// `did` — the full DID string, e.g. "did:aria:jayesh"
pub fn generate_identity(did: &str) -> anyhow::Result<Identity> {
    let device_secret = load_or_create_device_secret()?;
    let aes_key = derive_aes_key(&device_secret, did.as_bytes())?;

    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    let pub_multibase = multibase_encode(verifying_key.as_bytes());
    let priv_hex = encrypt_key_bytes(signing_key.as_bytes(), &aes_key)?;

    // Persist private key to separate file with 0600 permissions
    let path = identity_key_path()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(&path, &priv_hex)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    fs::write(&path, &priv_hex)?;

    Ok(Identity {
        did: did.to_string(),
        public_key_multibase: pub_multibase,
    })
}

/// Load the signing key from the `~/.aria/id.key` file.
pub fn load_signing_key(did: &str) -> anyhow::Result<SigningKey> {
    let path = identity_key_path()?;
    if !path.exists() {
        anyhow::bail!("Identity key file not found at {:?}", path);
    }
    let encrypted_hex = fs::read_to_string(path).context("reading id.key")?;

    let device_secret = load_or_create_device_secret()?;
    let aes_key = derive_aes_key(&device_secret, did.as_bytes())?;
    let raw = decrypt_key_bytes(encrypted_hex.trim(), &aes_key)?;
    if raw.len() != 32 {
        anyhow::bail!("decrypted signing key has wrong length: {}", raw.len());
    }
    let key_bytes: [u8; 32] = raw[..32].try_into().unwrap();
    Ok(SigningKey::from_bytes(&key_bytes))
}

// ── Multibase (base58btc) ─────────────────────────────────────────────────────

/// Encode bytes as multibase base58btc (prefix 'z').
/// Used in DID documents: `did:aria:jayesh#key-0` → `"z<base58(pubkey)>"`
pub fn multibase_encode(bytes: &[u8]) -> String {
    format!("z{}", bs58::encode(bytes).into_string())
}

/// Decode a multibase base58btc string (prefix 'z') back to bytes.
pub fn multibase_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    let s = s.strip_prefix('z').ok_or_else(|| anyhow!("not a base58btc multibase string"))?;
    bs58::decode(s).into_vec().context("base58 decode")
}

// ── Hashing ───────────────────────────────────────────────────────────────────

/// SHA-256 of arbitrary bytes → lowercase hex string.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// SHA-256 of a UTF-8 string → lowercase hex string.
pub fn sha256_hex_str(s: &str) -> String {
    sha256_hex(s.as_bytes())
}

/// Compute the chain hash for an audit log entry:
///   SHA-256(prev_hash || timestamp || skill_called || input_hash || result_hash)
/// Any field may be empty string if not applicable.
pub fn compute_chain_hash(
    prev_hash: &str,
    step: &str,
    skill_called: &str,
    input_hash: &str,
    result_hash: &str,
    timestamp: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(b"|");
    h.update(step.as_bytes());
    h.update(b"|");
    h.update(skill_called.as_bytes());
    h.update(b"|");
    h.update(input_hash.as_bytes());
    h.update(b"|");
    h.update(result_hash.as_bytes());
    h.update(b"|");
    h.update(timestamp.as_bytes());
    hex::encode(h.finalize())
}

// ── Signing / Verification ────────────────────────────────────────────────────

/// Sign `message` with the Ed25519 signing key → hex-encoded signature.
pub fn sign_bytes(signing_key: &SigningKey, message: &[u8]) -> String {
    hex::encode(signing_key.sign(message).to_bytes())
}

/// Verify an Ed25519 signature.
/// `public_key_multibase` — the 'z'-prefixed base58btc key from the identity row.
/// `message` — the bytes that were signed.
/// `signature_hex` — the hex signature from the audit log.
pub fn verify_signature(
    public_key_multibase: &str,
    message: &[u8],
    signature_hex: &str,
) -> anyhow::Result<()> {
    let pub_bytes = multibase_decode(public_key_multibase)?;
    let pub_bytes: [u8; 32] = pub_bytes
        .try_into()
        .map_err(|_| anyhow!("public key wrong length"))?;
    let verifying_key = VerifyingKey::from_bytes(&pub_bytes)?;

    let sig_bytes = hex::decode(signature_hex).context("hex decode signature")?;
    let sig_bytes: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| anyhow!("signature wrong length"))?;
    let signature = Signature::from_bytes(&sig_bytes);

    verifying_key
        .verify(message, &signature)
        .map_err(|_| anyhow!("signature verification failed"))?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_round_trip() {
        let h = sha256_hex_str("hello");
        assert_eq!(h.len(), 64);
        // known SHA-256 of "hello"
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_multibase_round_trip() {
        let data = b"test_public_key_bytes_32_pad_here";
        let enc = multibase_encode(data);
        assert!(enc.starts_with('z'));
        let dec = multibase_decode(&enc).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn test_sign_verify() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let pub_multibase = multibase_encode(verifying_key.as_bytes());

        let msg = b"test audit entry";
        let sig_hex = sign_bytes(&signing_key, msg);

        verify_signature(&pub_multibase, msg, &sig_hex).expect("signature should verify");
    }

    #[test]
    fn test_sign_tamper_detection() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let pub_multibase = multibase_encode(verifying_key.as_bytes());

        let sig_hex = sign_bytes(&signing_key, b"original message");
        let result = verify_signature(&pub_multibase, b"tampered message", &sig_hex);
        assert!(result.is_err(), "tampered message should not verify");
    }

    #[test]
    fn test_aes_gcm_round_trip() {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        let plaintext = b"ed25519 signing key bytes here!";
        let enc = encrypt_key_bytes(plaintext, &key).unwrap();
        let dec = decrypt_key_bytes(&enc, &key).unwrap();
        assert_eq!(dec.as_slice(), plaintext);
    }

    #[test]
    fn test_aes_gcm_wrong_key_fails() {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        let enc = encrypt_key_bytes(b"secret", &key).unwrap();
        let mut wrong_key = key;
        wrong_key[0] ^= 0xff;
        let result = decrypt_key_bytes(&enc, &wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_chain_hash_deterministic() {
        let h1 = compute_chain_hash("prev", "1", "search.web", "abc", "def", "2026-01-01T00:00:00Z");
        let h2 = compute_chain_hash("prev", "1", "search.web", "abc", "def", "2026-01-01T00:00:00Z");
        assert_eq!(h1, h2);
        let h3 = compute_chain_hash("prev", "1", "search.web", "abc", "DIFFERENT", "2026-01-01T00:00:00Z");
        assert_ne!(h1, h3);
    }
}
