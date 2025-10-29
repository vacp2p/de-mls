use openmls::{
    error::LibraryError,
    prelude::{CredentialError, KeyPackageNewError},
};
use openmls_rust_crypto::MemoryStorageError;
use openmls_traits::types::CryptoError;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error(transparent)]
    UnableToCreateKeyPackage(#[from] KeyPackageNewError),
    #[error("Invalid hash reference: {0}")]
    InvalidHashRef(#[from] LibraryError),
    #[error("Unable to create new signer: {0}")]
    UnableToCreateSigner(#[from] CryptoError),
    #[error("Unable to save signature key: {0}")]
    UnableToSaveSignatureKey(#[from] MemoryStorageError),
    #[error("Unable to create credential: {0}")]
    UnableToCreateCredential(#[from] CredentialError),
    #[error("Invalid wallet address: {0}")]
    InvalidWalletAddress(String),
}
