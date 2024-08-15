pub mod sc_ks;

use alloy::hex::FromHexError;

pub trait SCKeyStoreService {
    fn does_user_exist(
        &self,
        address: &str,
    ) -> impl std::future::Future<Output = Result<bool, KeyStoreError>>;
    fn add_user(
        &mut self,
        address: &str,
    ) -> impl std::future::Future<Output = Result<(), KeyStoreError>>;
    fn remove_user(
        &self,
        address: &str,
    ) -> impl std::future::Future<Output = Result<(), KeyStoreError>>;
}

#[derive(Debug, thiserror::Error)]
pub enum KeyStoreError {
    #[error("User already exists.")]
    UserAlreadyExistsError,

    #[error("User not found.")]
    UserNotFoundError,

    #[error("Alloy contract operation failed: {0}")]
    AlloyContractError(#[from] alloy::contract::Error),

    #[error("Failed to parse address: {0}")]
    AddressParseError(#[from] FromHexError),

    #[error("An unexpected error occurred: {0}")]
    UnexpectedError(anyhow::Error),
}
