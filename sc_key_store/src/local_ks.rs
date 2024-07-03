use std::collections::HashSet;

use openmls::prelude::KeyPackage;

use crate::{KeyStoreError, LocalKeyStoreService, SCKeyStoreService, UserInfo, UserKeyPackages};

pub struct LocalCache {
    user_info: UserInfo,
    // map of reserved key_packages [group_id, key_package_hash]
    pub reserved_key_pkg_hash: HashSet<Vec<u8>>,
}

impl LocalKeyStoreService for LocalCache {
    fn empty_key_store(id: &[u8]) -> Self {
        LocalCache {
            user_info: UserInfo {
                id: id.to_vec(),
                key_packages: UserKeyPackages::default(),
                sign_pk: Vec::with_capacity(32),
            },
            reserved_key_pkg_hash: HashSet::new(),
        }
    }

    async fn load_to_smart_contract<T: SCKeyStoreService>(
        &self,
        sc: &mut T,
    ) -> Result<(), KeyStoreError> {
        sc.add_user_kp(&self.user_info.id, self.user_info.key_packages.clone())
            .await
    }

    async fn get_update_from_smart_contract<T: SCKeyStoreService>(
        &mut self,
        sc: T,
    ) -> Result<(), KeyStoreError> {
        let info = sc.get_user(&self.user_info.id).await?;
        self.user_info.key_packages.clone_from(&info.key_packages);
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
