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

/// Information about a user.
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

impl From<UserKeyPackages> for UserInfo {
    fn from(mut key_packages: UserKeyPackages) -> Self {
        let key_package: KeyPackage = key_packages.0[0].1.clone();
        let id = key_package.leaf_node().credential().identity();
        let drain = key_packages.0.drain(..);
        Self {
            id: id.into(),
            key_packages: UserKeyPackages(drain.collect::<Vec<(Vec<u8>, KeyPackage)>>()),
            reserved_key_pkg_hash: HashSet::new(),
            msgs: Vec::new(),
            welcome_queue: Vec::new(),
            auth_token: AuthToken::random(),
        }
    }
}
