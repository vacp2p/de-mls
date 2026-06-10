//! Test-side wallet identity adapter.
//!
//! Bridges an Ethereum address to the library's identity-agnostic
//! interface: builds a [`WalletMemberId`] that implements
//! [`de_mls::member_id::MemberId`]. The low-level core fixtures use this to
//! key per-member MLS state. (Constructing a `User` from a private key lives
//! in the gateway test suite, which owns the reference integrator.)

use std::str::FromStr;

use alloy::primitives::Address;

use de_mls::member_id::MemberId;

/// Wallet-flavoured [`MemberId`] used by the core fixtures. Holds the
/// 20-byte Ethereum address bytes and its EIP-55 checksummed hex form.
#[derive(Debug, Clone)]
pub struct WalletMemberId {
    bytes: Vec<u8>,
    display: String,
}

impl WalletMemberId {
    /// Build from a parsed [`Address`].
    pub fn from_address(addr: Address) -> Self {
        Self {
            bytes: addr.as_slice().to_vec(),
            display: addr.to_checksum(None),
        }
    }

    /// Parse a `0x…`-prefixed wallet hex string.
    pub fn from_hex(hex: &str) -> Self {
        let addr = Address::from_str(hex.trim()).expect("valid wallet hex");
        Self::from_address(addr)
    }
}

impl MemberId for WalletMemberId {
    fn member_id_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn member_id_display(&self) -> &str {
        &self.display
    }
}
