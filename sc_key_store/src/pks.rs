use openmls::prelude::KeyPackage;
use std::collections::HashMap;

use mls_crypto::openmls_provider::MlsCryptoProvider;

use crate::{KeyStoreError, SCKeyStoreService, UserInfo, UserKeyPackages};

/// Public Key Storage
/// This is a tuple struct holding a vector of `(Vec<u8>, UserInfo)` tuples,
/// where the first value is the Ethereum wallet address of a user
/// and the second value is the corresponding user information.
#[derive(Debug, Default)]
pub struct PublicKeyStorage {
    storage: HashMap<Vec<u8>, UserInfo>,
}

impl From<UserKeyPackages> for UserInfo {
    fn from(mut key_packages: UserKeyPackages) -> Self {
        let key_package: KeyPackage = key_packages.0[0].1.clone();
        let id = key_package.leaf_node().credential().identity();
        let drain = key_packages.0.drain(..);
        Self {
            id: id.into(),
            key_packages: UserKeyPackages(drain.collect::<Vec<(Vec<u8>, KeyPackage)>>()),
            sign_pk: vec![0; 32],
        }
    }
}

impl SCKeyStoreService for &mut PublicKeyStorage {
    async fn does_user_exist(&self, id: &[u8]) -> Result<bool, KeyStoreError> {
        Ok(self.storage.contains_key(id))
    }

    async fn add_user(
        &mut self,
        ukp: UserKeyPackages,
        sign_pk: &[u8],
    ) -> Result<(), KeyStoreError> {
        if ukp.0.is_empty() {
            return Err(KeyStoreError::InvalidUserDataError(
                "no key packages".to_string(),
            ));
        }
        let mut new_user_info: UserInfo = ukp.into();
        new_user_info.sign_pk.clone_from_slice(sign_pk);

        if self.storage.contains_key(&new_user_info.id) {
            return Err(KeyStoreError::InvalidUserDataError(
                "already register".to_string(),
            ));
        }

        let res = self.storage.insert(new_user_info.id.clone(), new_user_info);
        assert!(res.is_none());

        Ok(())
    }

    async fn add_user_kp(&mut self, id: &[u8], ukp: UserKeyPackages) -> Result<(), KeyStoreError> {
        let user = match self.storage.get_mut(id) {
            Some(u) => u,
            None => return Err(KeyStoreError::UnknownUserError),
        };
        ukp.0
            .into_iter()
            .for_each(|value| user.key_packages.0.push(value));

        Ok(())
    }

    async fn get_user(
        &self,
        id: &[u8],
        _crypto: &MlsCryptoProvider,
    ) -> Result<UserInfo, KeyStoreError> {
        match self.storage.get(id) {
            Some(u) => Ok(u.to_owned()),
            None => Err(KeyStoreError::UnknownUserError),
        }
    }

    async fn get_avaliable_user_kp(
        &mut self,
        id: &[u8],
        _crypto: &MlsCryptoProvider,
    ) -> Result<KeyPackage, KeyStoreError> {
        let user = match self.storage.get_mut(id) {
            Some(u) => u,
            None => return Err(KeyStoreError::UnknownUserError),
        };
        match user.key_packages.0.pop() {
            Some(c) => Ok(c.1),
            None => Err(KeyStoreError::InvalidUserDataError(
                "No more keypackage available".to_string(),
            )),
        }
    }
}
