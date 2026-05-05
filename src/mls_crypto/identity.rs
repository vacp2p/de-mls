//! Identity sources for DE-MLS.
//!
//! [`IdentityProvider`] is the swap point for identity sources. The default
//! impl is [`WalletIdentity`], which derives MLS credentials from an
//! Ethereum wallet address. Future impls (libchat-style `AccountId`, etc.)
//! plug in by implementing the trait without touching the MLS service.

use std::str::FromStr;

use alloy::{hex, primitives::Address};
use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls_basic_credential::SignatureKeyPair;

use crate::mls_crypto::{IdentityError, service::CIPHERSUITE};

/// Pluggable identity source.
///
/// Identity bridges an authenticated user (wallet address, account id, …)
/// to the MLS credential system. The trait exposes the canonical identity
/// bytes plus a display form, and gives the OpenMLS-backed service access
/// to the credential and signer it needs.
///
/// All methods take `&self` so the trait stays object-safe; the
/// OpenMLS-backed service binds it as a generic field for monomorphization.
pub trait IdentityProvider: Send + Sync + 'static {
    /// Canonical identity bytes — what the credential's serialized content
    /// holds (e.g. wallet address bytes).
    fn identity_bytes(&self) -> &[u8];

    /// Display form (e.g. checksummed `0x…` hex). Stable for the lifetime
    /// of the identity.
    fn identity_display(&self) -> &str;

    /// MLS credential bundle — public part of the identity, embedded in
    /// every signed MLS message we produce.
    fn credential(&self) -> &CredentialWithKey;

    /// MLS signing key pair — owns the private key used to sign MLS
    /// messages and proposals.
    fn signer(&self) -> &SignatureKeyPair;
}

/// Wallet-based identity. The default [`IdentityProvider`] impl: derives an
/// MLS credential from a 20-byte Ethereum address, and generates a fresh
/// MLS signature key pair.
#[derive(Debug)]
pub struct WalletIdentity {
    wallet_bytes: Vec<u8>,
    wallet_hex: String,
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
}

impl WalletIdentity {
    /// Build a wallet identity from an Ethereum address. Generates a fresh
    /// signing keypair and bundles it with a basic credential containing the
    /// wallet bytes. Does not touch any MLS storage — the signer is held
    /// in-memory and passed explicitly into MLS calls that need it.
    pub fn from_wallet(wallet: Address) -> Result<Self, IdentityError> {
        let credential = BasicCredential::new(wallet.as_slice().to_vec());
        let signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm())?;
        Ok(Self {
            wallet_bytes: wallet.as_slice().to_vec(),
            wallet_hex: wallet.to_checksum(None),
            credential: CredentialWithKey {
                credential: credential.into(),
                signature_key: signer.to_public_vec().into(),
            },
            signer,
        })
    }
}

impl IdentityProvider for WalletIdentity {
    fn identity_bytes(&self) -> &[u8] {
        &self.wallet_bytes
    }

    fn identity_display(&self) -> &str {
        &self.wallet_hex
    }

    fn credential(&self) -> &CredentialWithKey {
        &self.credential
    }

    fn signer(&self) -> &SignatureKeyPair {
        &self.signer
    }
}

// ═══════════════════════════════════════════════════════════════
// Wallet Address Utilities
// ═══════════════════════════════════════════════════════════════

/// Parse an Ethereum wallet address string into an `Address`.
///
/// Accepts `0x`-prefixed or raw 40-character hex strings.
/// Validates that the address is exactly 20 bytes (40 hex chars).
///
/// # Examples
/// ```ignore
/// let addr = parse_wallet_address("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")?;
/// let addr = parse_wallet_address("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")?;
/// ```
pub fn parse_wallet_address(address: &str) -> Result<Address, IdentityError> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(IdentityError::InvalidWalletAddress(address.to_string()));
    }

    let hex_part = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);

    if hex_part.len() != 40 || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(IdentityError::InvalidWalletAddress(trimmed.to_string()));
    }

    let normalized = format!("0x{}", hex_part.to_ascii_lowercase());
    Address::from_str(&normalized)
        .map_err(|_| IdentityError::InvalidWalletAddress(trimmed.to_string()))
}

/// Format raw wallet bytes (20 bytes) as a hex string.
///
/// This is the inverse of `parse_wallet_address`. MLS credentials
/// store wallet addresses as raw 20-byte arrays; this formats them
/// for display.
///
/// # Examples
/// ```ignore
/// let bytes = [0x11, 0x22, ...]; // 20 bytes
/// let hex = format_wallet_address(&bytes); // "0x1122..."
/// ```
pub fn format_wallet_address(raw: &[u8]) -> String {
    if raw.is_empty() {
        String::new()
    } else {
        format!("0x{}", hex::encode(raw))
    }
}

/// Parse a wallet address string and return as raw bytes.
///
/// Combines `parse_wallet_address` with byte extraction.
/// Useful for creating removal requests from user input.
pub fn parse_wallet_to_bytes(address: &str) -> Result<Vec<u8>, IdentityError> {
    let addr = parse_wallet_address(address)?;
    Ok(addr.as_slice().to_vec())
}

/// Short identity prefix for log output. Renders the first 4 bytes as hex
/// (8 chars) — long enough to be unique across the group, short enough not
/// to dominate a log line. Use as `%ShortId(&identity)` in `tracing` macros.
pub struct ShortId<'a>(pub &'a [u8]);

impl std::fmt::Display for ShortId<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return f.write_str("∅");
        }
        for b in self.0.iter().take(4) {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prefixed_address() {
        let addr = parse_wallet_address("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap();
        assert_eq!(
            addr.to_string().to_lowercase(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }

    #[test]
    fn parse_unprefixed_address() {
        let addr = parse_wallet_address("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap();
        assert_eq!(
            addr.to_string().to_lowercase(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }

    #[test]
    fn parse_with_whitespace() {
        let addr = parse_wallet_address("  0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266  ").unwrap();
        assert_eq!(
            addr.to_string().to_lowercase(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }

    #[test]
    fn reject_invalid_length() {
        assert!(parse_wallet_address("0x1234").is_err());
        assert!(parse_wallet_address("0x").is_err());
        assert!(parse_wallet_address("").is_err());
    }

    #[test]
    fn reject_invalid_chars() {
        assert!(parse_wallet_address("0xGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG").is_err());
    }

    #[test]
    fn format_bytes_to_hex() {
        let bytes = [0x11, 0x22, 0x33];
        assert_eq!(format_wallet_address(&bytes), "0x112233");
    }

    #[test]
    fn format_empty_bytes() {
        assert_eq!(format_wallet_address(&[]), "");
    }

    #[test]
    fn roundtrip() {
        let original = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
        let addr = parse_wallet_address(original).unwrap();
        let formatted = format_wallet_address(addr.as_slice());
        assert_eq!(formatted.to_lowercase(), original.to_lowercase());
    }
}
