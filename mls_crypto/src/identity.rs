use alloy::{hex, primitives::Address, signers::local::PrivateKeySigner};
use openmls::{credentials::CredentialWithKey, key_packages::KeyPackage, prelude::BasicCredential};
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::{types::Ciphersuite, OpenMlsProvider};
use std::{collections::HashMap, fmt::Display, str::FromStr};

use crate::error::IdentityError;
use crate::openmls_provider::{MlsProvider, CIPHERSUITE};

pub struct Identity {
    pub(crate) kp: HashMap<Vec<u8>, KeyPackage>,
    pub(crate) credential_with_key: CredentialWithKey,
    pub(crate) signer: SignatureKeyPair,
}

impl Identity {
    pub fn new(
        ciphersuite: Ciphersuite,
        provider: &MlsProvider,
        user_wallet_address: &[u8],
    ) -> Result<Identity, IdentityError> {
        let credential = BasicCredential::new(user_wallet_address.to_vec());
        let signer = SignatureKeyPair::new(ciphersuite.signature_algorithm())?;
        let credential_with_key = CredentialWithKey {
            credential: credential.into(),
            signature_key: signer.to_public_vec().into(),
        };
        signer.store(provider.storage())?;

        let mut kps = HashMap::new();
        let key_package_bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            provider,
            &signer,
            credential_with_key.clone(),
        )?;
        let key_package = key_package_bundle.key_package();
        let kp = key_package.hash_ref(provider.crypto())?;
        kps.insert(kp.as_slice().to_vec(), key_package.clone());

        Ok(Identity {
            kp: kps,
            credential_with_key,
            signer,
        })
    }

    /// Create an additional key package using the credential_with_key/signer bound to this identity
    pub fn generate_key_package(
        &mut self,
        crypto: &MlsProvider,
    ) -> Result<KeyPackage, IdentityError> {
        let key_package_bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            crypto,
            &self.signer,
            self.credential_with_key.clone(),
        )?;
        let key_package = key_package_bundle.key_package();
        let kp = key_package.hash_ref(crypto.crypto())?;
        self.kp.insert(kp.as_slice().to_vec(), key_package.clone());
        Ok(key_package.clone())
    }

    /// Get the plain identity as byte vector.
    pub fn identity(&self) -> &[u8] {
        self.credential_with_key.credential.serialized_content()
    }

    pub fn identity_string(&self) -> String {
        Address::from_slice(self.credential_with_key.credential.serialized_content()).to_string()
    }

    pub fn signer(&self) -> &SignatureKeyPair {
        &self.signer
    }

    pub fn credential_with_key(&self) -> CredentialWithKey {
        self.credential_with_key.clone()
    }

    pub fn signature_key(&self) -> Vec<u8> {
        self.credential_with_key.signature_key.as_slice().to_vec()
    }

    pub fn is_key_package_exists(&self, kp_hash_ref: &[u8]) -> bool {
        self.kp.contains_key(kp_hash_ref)
    }
}

impl Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.identity_string())
    }
}

pub fn random_identity() -> Result<Identity, IdentityError> {
    let signer = PrivateKeySigner::random();
    let user_address = signer.address();

    let crypto = MlsProvider::default();
    let id = Identity::new(CIPHERSUITE, &crypto, user_address.as_slice())?;
    Ok(id)
}

/// Validates and normalizes Ethereum-style wallet addresses.
///
/// Accepts either `0x`-prefixed or raw 40-character hex strings, returning a lowercase,
/// `0x`-prefixed representation on success.
pub fn normalize_wallet_address_str(address: &str) -> Result<String, IdentityError> {
    parse_wallet_address(address).map(|addr| addr.to_string())
}

/// Parses an Ethereum wallet address into an [`Address`] after validation.
///
/// This ensures the address is 20 bytes / 40 hex chars and contains only hexadecimal digits.
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

fn is_prefixed_hex(input: &str) -> bool {
    let rest = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"));
    match rest {
        Some(hex_part) if !hex_part.is_empty() => hex_part.chars().all(|c| c.is_ascii_hexdigit()),
        _ => false,
    }
}

fn is_raw_hex(input: &str) -> bool {
    !input.is_empty() && input.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn normalize_wallet_address(raw: &[u8]) -> String {
    let as_utf8 = std::str::from_utf8(raw)
        .map(|s| s.trim())
        .unwrap_or_default();

    if is_prefixed_hex(as_utf8) {
        return as_utf8.to_string();
    }

    if is_raw_hex(as_utf8) {
        return format!("0x{}", as_utf8);
    }

    if raw.is_empty() {
        String::new()
    } else {
        format!("0x{}", hex::encode(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::{is_prefixed_hex, normalize_wallet_address};

    #[test]
    fn keeps_prefixed_hex() {
        let addr = normalize_wallet_address(b"0xAbCd1234");
        assert_eq!(addr, "0xAbCd1234");
    }

    #[test]
    fn prefixes_raw_hex() {
        let addr = normalize_wallet_address(b"ABCD1234");
        assert_eq!(addr, "0xABCD1234");
    }

    #[test]
    fn encodes_binary_bytes() {
        let addr = normalize_wallet_address(&[0x11, 0x22, 0x33]);
        assert_eq!(addr, "0x112233");
    }

    #[test]
    fn trims_ascii_input() {
        let addr = normalize_wallet_address(b"  0x1F  ");
        assert_eq!(addr, "0x1F");
    }

    #[test]
    fn prefixed_hex_helper() {
        assert!(is_prefixed_hex("0xabc"));
        assert!(is_prefixed_hex("0XABC"));
        assert!(!is_prefixed_hex("abc"));
        assert!(!is_prefixed_hex("0x"));
    }
}
