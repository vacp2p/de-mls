use openmls::{
    error::LibraryError,
    prelude::{CredentialError, KeyPackageNewError},
};
use openmls_rust_crypto::MemoryStorageError;
use openmls_traits::types::CryptoError;

pub mod identity;
pub mod openmls_provider;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("Failed to create new key package: {0}")]
    MlsKeyPackageCreationError(#[from] KeyPackageNewError),
    #[error(transparent)]
    MlsLibraryError(#[from] LibraryError),
    #[error(transparent)]
    MlsCryptoError(#[from] CryptoError),
    #[error("Failed to save signature key: {0}")]
    MlsKeyStoreError(#[from] MemoryStorageError),
    #[error("Failed to create credential: {0}")]
    MlsCredentialError(#[from] CredentialError),
    #[error("Invalid key")]
    InvalidKey,
    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
