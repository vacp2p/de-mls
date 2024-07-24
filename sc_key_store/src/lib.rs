pub mod sc_ks;

use mls_crypto::openmls_provider::MlsCryptoProvider;
use openmls::prelude::*;
use std::collections::HashSet;

/// The DS returns a list of key packages for a user as `UserKeyPackages`.
/// This is a tuple struct holding a vector of `(Vec<u8>, KeyPackage)` tuples,
/// where the first value is the key package hash (output of `KeyPackage::hash`)
/// and the second value is the corresponding key package.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UserKeyPackages(pub Vec<(Vec<u8>, KeyPackage)>);

impl UserKeyPackages {
    fn get_id_from_kp(&self) -> Vec<u8> {
        let key_package: KeyPackage = self.0[0].1.clone();
        key_package.leaf_node().credential().identity().to_vec()
    }
}

/// Information about a user.
#[derive(Debug, Default, Clone)]
pub struct UserInfo {
    pub id: Vec<u8>,
    pub key_packages: UserKeyPackages,
    pub key_packages_hash: HashSet<Vec<u8>>,
    pub sign_pk: Vec<u8>,
}

pub trait SCKeyStoreService {
    fn does_user_exist(
        &self,
        id: &[u8],
    ) -> impl std::future::Future<Output = Result<bool, KeyStoreError>>;
    fn add_user(
        &mut self,
        ukp: UserKeyPackages,
        sign_pk: &[u8],
    ) -> impl std::future::Future<Output = Result<(), KeyStoreError>>;
    fn get_user(
        &self,
        id: &[u8],
        crypto: &MlsCryptoProvider,
    ) -> impl std::future::Future<Output = Result<UserInfo, KeyStoreError>>;
    fn add_user_kp(
        &mut self,
        id: &[u8],
        ukp: UserKeyPackages,
    ) -> impl std::future::Future<Output = Result<(), KeyStoreError>>;
    // we need get key package of other user for inviting them to group
    fn get_avaliable_user_kp(
        &mut self,
        id: &[u8],
        crypto: &MlsCryptoProvider,
    ) -> impl std::future::Future<Output = Result<KeyPackage, KeyStoreError>>;
}

#[derive(Debug, thiserror::Error)]
pub enum KeyStoreError {
    #[error("User doesn't exist")]
    UnknownUserError,
    #[error("Invalid data for User: {0}")]
    InvalidUserDataError(String),
    #[error("Unauthorized User")]
    UnauthorizedUserError,
    #[error("User already exist")]
    AlreadyExistedUserError,
    #[error("Alloy contract error: {0}")]
    AlloyError(#[from] alloy::contract::Error),
    #[error("Serialization problem: {0}")]
    TlsError(#[from] tls_codec::Error),
    #[error("Key package doesn't valid: {0}")]
    MlsKeyPackageVerifyError(#[from] openmls::prelude::KeyPackageVerifyError),
    #[error(transparent)]
    MlsLibraryError(#[from] LibraryError),
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
