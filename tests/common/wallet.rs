//! Test-side wallet identity.
//!
//! An integrator brings its own identity; the fixtures use an Ethereum address
//! as one. It is what the test credential carries and what a conversation's
//! `app_id` is taken from.
use alloy::primitives::Address;

/// The wallet a fixture speaks for: the 20-byte Ethereum address and its
/// EIP-55 checksummed hex form.
#[derive(Debug, Clone)]
pub struct WalletIdentity {
    bytes: Vec<u8>,
    display: String,
}

impl WalletIdentity {
    /// Build from a parsed [`Address`].
    pub fn from_address(addr: Address) -> Self {
        Self {
            bytes: addr.as_slice().to_vec(),
            display: addr.to_checksum(None),
        }
    }

    /// The raw address bytes: the identity the test credential carries.
    pub fn address_bytes(&self) -> &[u8] {
        &self.bytes
    }
}
