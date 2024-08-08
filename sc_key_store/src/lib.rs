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
    #[error("User already exist")]
    AlreadyExistedUserError,
    #[error("Unknown user")]
    UnknownUserError,
    #[error("Alloy contract error: {0}")]
    AlloyError(#[from] alloy::contract::Error),
    #[error("Unable to parce the address: {0}")]
    AlloyFromHexError(#[from] FromHexError),
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
