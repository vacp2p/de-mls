use libsecp256k1::{PublicKey, SecretKey};
use openmls::prelude::*;

use crate::*;

#[derive(Clone, Debug)]
pub struct GroupAdmin {
    public_key: PublicKey,
    private_key: SecretKey,
    incoming_key_packages: Arc<Mutex<Vec<KeyPackage>>>,
}

impl Default for GroupAdmin {
    fn default() -> Self {
        Self::new_admin()
    }
}

impl GroupAdmin {
    pub fn new_admin() -> Self {
        let (public_key, private_key) = generate_keypair();
        GroupAdmin {
            public_key,
            private_key,
            incoming_key_packages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn refresh_key_pair(&mut self) {
        let (public_key, private_key) = generate_keypair();
        self.public_key = public_key;
        self.private_key = private_key;
    }

    pub fn create_admin_announcement(&self) -> GroupAnnouncement {
        let signature = sign_message(&self.public_key.serialize_compressed(), &self.private_key);
        GroupAnnouncement::new(self.public_key.serialize_compressed().to_vec(), signature)
    }

    pub fn decrypt_message(&self, message: Vec<u8>) -> Result<KeyPackage, MessageError> {
        let msg: Vec<u8> = decrypt_message(&message, self.private_key)?;
        let key_package: KeyPackage = serde_json::from_slice(&msg)?;
        Ok(key_package)
    }

    pub fn add_incoming_key_package(&mut self, key_package: KeyPackage) {
        self.incoming_key_packages.lock().unwrap().push(key_package)
    }

    pub fn drain_processed_key_packages(&mut self) -> Vec<KeyPackage> {
        self.incoming_key_packages
            .lock()
            .unwrap()
            .drain(0..)
            .collect()
    }
}
