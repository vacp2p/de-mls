use std::collections::HashMap;

use openmls::prelude_test::KeyPackage;

use crate::{AuthToken, UserInfo, UserKeyPackages};

/// Public Key Storage
/// This is a tuple struct holding a vector of `(Vec<u8>, UserInfo)` tuples,
/// where the first value is the Ethereum wallet address of a user
/// and the second value is the corresponding user information.
#[derive(Debug, Default)]
pub struct PublicKeyStorage {
    storage: HashMap<Vec<u8>, UserInfo>,
}

impl PublicKeyStorage {
    pub fn new() -> Self {
        PublicKeyStorage::default()
    }

    pub fn does_user_exist(&self, id: &[u8]) -> bool {
        self.storage.contains_key(id)
    }

    pub fn add_user(&mut self, key_packages: UserKeyPackages) -> Result<AuthToken, KeyStoreError> {
        if key_packages.0.is_empty() {
            return Err(KeyStoreError::InvalidUserDataError(
                "no key packages".to_string(),
            ));
        }
        let new_user_info: UserInfo = key_packages.into();

        if self.storage.contains_key(&new_user_info.id) {
            return Err(KeyStoreError::InvalidUserDataError(
                "already register".to_string(),
            ));
        }

        let res = self
            .storage
            .insert(new_user_info.id.clone(), new_user_info.clone());
        assert!(res.is_none());

        Ok(new_user_info.auth_token)
    }

    pub fn get_avaliable_user_kp(&mut self, id: &[u8]) -> Result<KeyPackage, KeyStoreError> {
        if !self.storage.contains_key(id) {
            return Err(KeyStoreError::UnknownUserError);
        }
        let user = self.storage.get_mut(id).unwrap();
        match user.key_packages.0.pop() {
            Some(c) => Ok(c.1),
            None => Err(KeyStoreError::InvalidUserDataError(
                "No more keypackage available".to_string(),
            )),
        }
    }

    pub fn add_user_kp(
        &mut self,
        id: &[u8],
        auth_token: AuthToken,
        ukp: UserKeyPackages,
    ) -> Result<(), KeyStoreError> {
        if !self.storage.contains_key(id) {
            return Err(KeyStoreError::UnknownUserError);
        }
        let user = self.storage.get_mut(id).unwrap();

        if auth_token != user.auth_token {
            return Err(KeyStoreError::UnauthorizedUserError);
        }

        ukp.0
            .into_iter()
            .for_each(|value| user.key_packages.0.push(value));

        Ok(())
    }

    pub fn get_user_auth_token(&self, id: &[u8]) -> Result<&AuthToken, KeyStoreError> {
        if !self.storage.contains_key(id) {
            return Err(KeyStoreError::UnknownUserError);
        }
        let user = self.storage.get(id).unwrap();
        Ok(&user.auth_token)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyStoreError {
    #[error("User doesn't exist")]
    UnknownUserError,
    #[error("Invalid data for User: {0}")]
    InvalidUserDataError(String),
    #[error("Unauthorized User")]
    UnauthorizedUserError,
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
