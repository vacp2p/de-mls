use openmls::prelude_test::KeyPackage;
// use std::sync::Arc;
use std::collections::HashMap;
use std::sync::Mutex;
// use tokio::sync::Mutex;

use crate::{AuthToken, UserInfo, UserKeyPackages};

/// Public Key Storage
/// This is a tuple struct holding a vector of `(Vec<u8>, UserInfo)` tuples,
/// where the first value is the Ethereum wallet address of a user
/// and the second value is the corresponding user information.
#[derive(Debug, Default)]
pub struct PublicKeyStorage {
    storage: Mutex<HashMap<Vec<u8>, UserInfo>>,
}

impl PublicKeyStorage {
    pub fn new() -> Self {
        PublicKeyStorage::default()
    }

    pub fn is_client_exist(&self, id: Vec<u8>) -> Result<(), String> {
        let storage = self.storage.lock().unwrap();
        if storage.contains_key(&id) {
            Ok(())
        } else {
            Err("Client doesn't exist".to_string())
        }
    }

    pub fn add_user(&self, key_packages: UserKeyPackages) -> Result<AuthToken, String> {
        if key_packages.0.is_empty() {
            return Err("Invalid data for client: no key packages".to_string());
        }
        let new_client_info = UserInfo::new(key_packages.0);
        println!("Registering client: {:?}", new_client_info.id);

        let mut clients = self.storage.lock().unwrap();
        if clients.contains_key(&new_client_info.id) {
            return Err("Invalid data for client: already register".to_string());
        }

        let res = clients.insert(new_client_info.id.clone(), new_client_info.clone());
        assert!(res.is_none());

        Ok(new_client_info.auth_token)
    }

    pub fn get_user_kp(&self, id: Vec<u8>) -> Result<KeyPackage, String> {
        let mut storage = self.storage.lock().unwrap();
        if !storage.contains_key(&id) {
            return Err("Client doesn't exist".to_string());
        }

        let user = match storage.get_mut(&id) {
            Some(c) => c,
            None => return Err("No client found".to_string()),
        };

        let kp = user.get_kp().unwrap().clone();
        Ok(kp)
    }

    pub fn add_user_kp(
        &self,
        id: Vec<u8>,
        auth_token: AuthToken,
        ukp: UserKeyPackages,
    ) -> Result<(), String> {
        let mut storage = self.storage.lock().unwrap();
        if !storage.contains_key(&id) {
            return Err("Client doesn't exist".to_string());
        }
        let user = match storage.get_mut(&id) {
            Some(c) => c,
            None => return Err("No client found".to_string()),
        };

        if auth_token != user.auth_token {
            return Err("Unauthorized client".to_string());
        }

        ukp.0
            .into_iter()
            .for_each(|value| user.key_packages.0.push(value));

        Ok(())
    }

    pub fn get_user_auth_token(&self, id: Vec<u8>) -> Result<AuthToken, String> {
        let user = self.storage.lock().unwrap();
        if !user.contains_key(&id) {
            return Err("Client doesn't exist".to_string());
        }
        let user = match user.get(&id) {
            Some(c) => c,
            None => return Err("No client found".to_string()),
        };
        let at = user.auth_token.clone();
        Ok(at)
    }
}
