//! Ethereum wallet-based identity for DE-MLS.

use alloy::{hex, primitives::Address};
use openmls::credentials::CredentialWithKey;
use openmls_basic_credential::SignatureKeyPair;
use std::str::FromStr;

use crate::mls_crypto::IdentityError;

/// Identity data held in memory (internal).
#[derive(Debug)]
pub(crate) struct IdentityData {
    /// Ethereum wallet address (20 bytes).
    pub wallet: Address,
    /// MLS credential with public signature key.
    pub credential: CredentialWithKey,
    /// MLS signature key pair (includes private key).
    pub signer: SignatureKeyPair,
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
