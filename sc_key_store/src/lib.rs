pub mod local_ks;
pub mod pks;
pub mod sc_ks;

use openmls::prelude::*;

/// The DS returns a list of key packages for a user as `UserKeyPackages`.
/// This is a tuple struct holding a vector of `(Vec<u8>, KeyPackage)` tuples,
/// where the first value is the key package hash (output of `KeyPackage::hash`)
/// and the second value is the corresponding key package.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UserKeyPackages(pub Vec<(Vec<u8>, KeyPackage)>);

/// Information about a user.
#[derive(Debug, Default, Clone)]
pub struct UserInfo {
    pub id: Vec<u8>,
    pub key_packages: UserKeyPackages,
    pub sign_pk: Vec<u8>,
}

pub trait SCKeyStoreService {
    async fn does_user_exist(&self, id: &[u8]) -> Result<bool, KeyStoreError>;
    async fn add_user(&mut self, ukp: UserKeyPackages, sign_pk: &[u8])
        -> Result<(), KeyStoreError>;
    async fn get_user(&self, id: &[u8]) -> Result<UserInfo, KeyStoreError>;
    async fn add_user_kp(&mut self, id: &[u8], ukp: UserKeyPackages) -> Result<(), KeyStoreError>;
    // we need get key package of other user for inviting them to group
    async fn get_avaliable_user_kp(&mut self, id: &[u8]) -> Result<KeyPackage, KeyStoreError>;
}

pub trait LocalKeyStoreService {
    fn empty_key_store(id: &[u8]) -> Self;

    async fn load_to_smart_contract<T: SCKeyStoreService>(
        &self,
        sc: &mut T,
    ) -> Result<(), KeyStoreError>;
    async fn get_update_from_smart_contract<T: SCKeyStoreService>(
        &mut self,
        sc: T,
    ) -> Result<(), KeyStoreError>;

    fn get_avaliable_kp(&mut self) -> Result<KeyPackage, KeyStoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum KeyStoreError {
    #[error("User doesn't exist")]
    UnknownUserError,
    #[error("Invalid data for User: {0}")]
    InvalidUserDataError(String),
    #[error("Unauthorized User")]
    UnauthorizedUserError,
    #[error("Alloy contract error: {0}")]
    AlloyError(#[from] alloy::contract::Error),
    #[error("Serialization problem: {0}")]
    TlsError(#[from] tls_codec::Error),
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
