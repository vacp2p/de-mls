use std::collections::HashSet;

use mls_crypto::openmls_provider::MlsCryptoProvider;
use openmls::prelude::KeyPackage;

use crate::{KeyStoreError, SCKeyStoreService, UserInfo, UserKeyPackages};

pub struct LocalCache {
    user_info: UserInfo,
    // map of reserved key_packages [group_id, key_package_hash]
    pub reserved_key_pkg_hash: HashSet<Vec<u8>>,
}

impl LocalCache {
    fn empty_key_store(id: &[u8], sign_pk: &[u8]) -> Self {
        LocalCache {
            user_info: UserInfo {
                id: id.to_vec(),
                key_packages: UserKeyPackages::default(),
                key_packages_hash: HashSet::default(),
                sign_pk: sign_pk.to_vec(),
            },
            reserved_key_pkg_hash: HashSet::new(),
        }
    }

    fn fill_empty_key_store(&mut self, ukp: UserKeyPackages) -> Result<(), KeyStoreError> {
        let ukp_hash: HashSet<Vec<u8>> = ukp.0.iter().map(|(k, _)| k.to_owned()).collect();
        self.user_info.key_packages.clone_from(&ukp);
        self.user_info.key_packages_hash.clone_from(&ukp_hash);
        Ok(())
    }

    async fn load_to_smart_contract<T: SCKeyStoreService>(
        &self,
        sc: &mut T,
    ) -> Result<(), KeyStoreError> {
        sc.add_user_kp(&self.user_info.id, self.user_info.key_packages.clone())
            .await
    }

    // TODO: add a check if the key has been deleted and update the local data
    async fn get_update_from_smart_contract<T: SCKeyStoreService>(
        &mut self,
        sc: T,
        crypto: &MlsCryptoProvider,
    ) -> Result<(), KeyStoreError> {
        let info = sc.get_user(&self.user_info.id, crypto).await?;
        let ukp_from_sc = &info.key_packages;
        let ukp_hash: HashSet<Vec<u8>> = ukp_from_sc.0.iter().map(|(k, _)| k.to_owned()).collect();

        self.user_info.key_packages.clone_from(ukp_from_sc);
        self.reserved_key_pkg_hash.clone_from(&ukp_hash);
        Ok(())
    }

    fn get_avaliable_kp(&mut self) -> Result<KeyPackage, KeyStoreError> {
        match self.user_info.key_packages.0.pop() {
            Some(c) => Ok(c.1),
            None => Err(KeyStoreError::InvalidUserDataError(
                "No more keypackage available".to_string(),
            )),
        }
    }
}
