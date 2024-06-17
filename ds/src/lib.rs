pub mod ds;
pub mod keystore;

use std::collections::HashSet;

use openmls::prelude::*;
use rand::{thread_rng, Rng};

/// The DS returns a list of key packages for a user as `UserKeyPackages`.
/// This is a tuple struct holding a vector of `(Vec<u8>, KeyPackage)` tuples,
/// where the first value is the key package hash (output of `KeyPackage::hash`)
/// and the second value is the corresponding key package.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UserKeyPackages(pub Vec<(Vec<u8>, KeyPackage)>);

#[derive(Debug, Clone, PartialEq)]
pub struct AuthToken {
    token: Vec<u8>,
}

impl Default for AuthToken {
    fn default() -> Self {
        Self::random()
    }
}

impl AuthToken {
    pub fn random() -> Self {
        let token = thread_rng().gen::<[u8; 32]>().to_vec();
        Self { token }
    }
}

/// Information about a client.
#[derive(Debug, Default, Clone)]
pub struct UserInfo {
    pub id: Vec<u8>,
    pub key_packages: UserKeyPackages,
    /// map of reserved key_packages [group_id, key_package_hash]
    pub reserved_key_pkg_hash: HashSet<Vec<u8>>,
    pub msgs: Vec<MlsMessageIn>,
    pub welcome_queue: Vec<MlsMessageIn>,
    pub auth_token: AuthToken,
}

impl UserInfo {
    /// Create a new `ClientInfo` struct for a given client name and vector of
    /// key packages with corresponding hashes.
    pub fn new(mut key_packages: Vec<(Vec<u8>, KeyPackage)>) -> Self {
        let key_package: KeyPackage = key_packages[0].1.clone();
        let id = key_package.leaf_node().credential().identity();
        let drain = key_packages.drain(..);
        Self {
            id: id.into(),
            key_packages: UserKeyPackages(drain.collect::<Vec<(Vec<u8>, KeyPackage)>>()),
            reserved_key_pkg_hash: HashSet::new(),
            msgs: Vec::new(),
            welcome_queue: Vec::new(),
            auth_token: AuthToken::random(),
        }
    }

    /// The identity of a client is defined as the identity of the first key
    /// package right now.
    pub fn id(&self) -> &[u8] {
        self.id.as_slice()
    }

    pub fn get_kp(&mut self) -> Result<KeyPackage, String> {
        match self.key_packages.0.pop() {
            Some(c) => Ok(c.1),
            None => Err("No more keypackage available".to_string()),
        }
    }
}
