use chrono::Utc;
use libsecp256k1::{PublicKey, SecretKey};
use openmls::prelude::*;

use crate::*;

#[derive(Clone, Debug)]
pub struct Admin {
    current_key_pair: PublicKey,
    current_key_pair_private: SecretKey,
    key_pair_timestamp: u64,
    income_key_package: Vec<KeyPackage>,
    processed_key_package: Vec<KeyPackage>,
}

impl Default for Admin {
    fn default() -> Self {
        Self::new()
    }
}

impl Admin {
    pub fn new() -> Self {
        let (public_key, secret_key) = generate_keypair();
        Admin {
            current_key_pair: public_key,
            current_key_pair_private: secret_key,
            key_pair_timestamp: Utc::now().timestamp() as u64,
            income_key_package: Vec::new(),
            processed_key_package: Vec::new(),
        }
    }

    pub fn generate_new_key_pair(&mut self) {
        let (public_key, secret_key) = generate_keypair();
        self.current_key_pair = public_key;
        self.current_key_pair_private = secret_key;
        self.key_pair_timestamp = Utc::now().timestamp() as u64;
    }

    pub fn generate_admin_message(&self) -> GroupAnnouncement {
        let signature = sign_message(
            &self.current_key_pair.serialize_compressed(),
            &self.current_key_pair_private,
        );
        GroupAnnouncement::new(
            self.current_key_pair.serialize_compressed().to_vec(),
            signature,
        )
    }

    pub fn decrypt_msg(&self, message: Vec<u8>) -> Result<KeyPackage, MessageError> {
        let msg: Vec<u8> = decrypt_message(&message, self.current_key_pair_private)?;
        let key_package: KeyPackage = serde_json::from_slice(&msg)?;
        Ok(key_package)
    }

    pub fn add_income_key_package(&mut self, key_package: KeyPackage) {
        self.income_key_package.push(key_package);
    }

    pub fn move_income_key_package_to_processed(&mut self) {
        self.processed_key_package
            .extend(self.income_key_package.clone());
        self.income_key_package.clear();
    }

    pub fn processed_key_packages(&mut self) -> Vec<KeyPackage> {
        self.processed_key_package.drain(0..).collect()
    }
}
