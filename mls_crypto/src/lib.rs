use openmls::{error::LibraryError, prelude::*};
use openmls_rust_crypto::MemoryKeyStoreError;

pub mod identity;
pub mod openmls_provider;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("Failed to create new key package: {0}")]
    MlsKeyPackageCreationError(#[from] KeyPackageNewError<MemoryKeyStoreError>),
    #[error(transparent)]
    MlsLibraryError(#[from] LibraryError),
    #[error("Failed to create signature: {0}")]
    MlsCryptoError(#[from] CryptoError),
    #[error("Failed to save signature key: {0}")]
    MlsKeyStoreError(#[from] MemoryKeyStoreError),
    #[error("Failed to create credential: {0}")]
    MlsCredentialError(#[from] CredentialError),
    #[error("Invalid key")]
    InvalidKey,
    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
