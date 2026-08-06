//! Abstraction over who pays Hedera transaction fees for ARIA's DID operations.
//!
//! Today ARIA uses the user-supplied ECDSA operator account locally. The
//! future model relays DID operations to a server which signs nothing — ARIA
//! signs locally with `id.key` and the relay pays the fee. Keeping the fee
//! payer behind this enum means that swap stays a one-file change.
//!
//! The ECDSA key is never the DID's control key: it only funds gas. The DID's
//! root/verification key is the Ed25519 key stored in `~/.aria/id.key`.

use hiero_sdk::{
    AccountId,
    Client,
    PrivateKey,
};

/// Who pays the HCS gas for creating / updating the did:hedera document.
pub enum FeePayer {
    /// The user's ECDSA operator account, provided via env or prompt.
    Local(Client, AccountId, PrivateKey),
    /// Future: a relay server that submits the locally-signed operation and
    /// pays the fee. Not implemented yet.
    Remote { endpoint: reqwest::Url },
}

impl FeePayer {
    /// The operator client used to pay HCS fees today.
    ///
    /// # Panics
    /// Panics for `FeePayer::Remote`, which is not implemented yet.
    pub fn client(&self) -> &Client {
        match self {
            FeePayer::Local(client, _, _) => client,
            FeePayer::Remote { .. } => {
                unimplemented!("FeePayer::Remote (relay-server fee payment) is not implemented yet")
            }
        }
    }
}
