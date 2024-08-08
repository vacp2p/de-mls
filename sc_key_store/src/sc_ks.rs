use alloy::{network::Network, primitives::Address, providers::Provider, transports::Transport};
use foundry_contracts::sckeystore::ScKeystore::{self, ScKeystoreInstance};
use std::str::FromStr;

use crate::{KeyStoreError, SCKeyStoreService};

pub struct ScKeyStorage<T, P, N> {
    instance: ScKeystoreInstance<T, P, N>,
    address: String,
}

impl<T, P, N> ScKeyStorage<T, P, N>
where
    T: Transport + Clone,
    P: Provider<T, N>,
    N: Network,
{
    pub fn new(provider: P, address: Address) -> Self {
        Self {
            instance: ScKeystore::new(address, provider),
            address: address.to_string(),
        }
    }

    pub fn get_sc_adsress(&self) -> String {
        self.address.clone()
    }
}

impl<T: Transport + Clone, P: Provider<T, N>, N: Network> SCKeyStoreService
    for ScKeyStorage<T, P, N>
{
    async fn does_user_exist(&self, address: &str) -> Result<bool, KeyStoreError> {
        let address = Address::from_str(address)?;
        let res = self.instance.userExists(address).call().await;
        match res {
            Ok(is_exist) => Ok(is_exist._0),
            Err(err) => Err(KeyStoreError::AlloyError(err)),
        }
    }

    async fn add_user(&mut self, address: &str) -> Result<(), KeyStoreError> {
        if self.does_user_exist(address).await? {
            return Err(KeyStoreError::AlreadyExistedUserError);
        }

        let add_to_acl_binding = self.instance.addUser(Address::from_str(address)?);
        let res = add_to_acl_binding.send().await;
        match res {
            Ok(_) => Ok(()),
            Err(err) => Err(KeyStoreError::AlloyError(err)),
        }
    }

    async fn remove_user(&self, address: &str) -> Result<(), KeyStoreError> {
        if !self.does_user_exist(address).await? {
            return Err(KeyStoreError::UnknownUserError);
        }

        let remove_user_binding = self.instance.removeUser(Address::from_str(address)?);
        let res = remove_user_binding.send().await;
        match res {
            Ok(_) => Ok(()),
            Err(err) => Err(KeyStoreError::AlloyError(err)),
        }
    }
}
