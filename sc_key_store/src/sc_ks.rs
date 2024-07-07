use alloy::{
    network::{EthereumWallet, Network},
    primitives::{Address, Bytes},
    providers::Provider,
    signers::local::PrivateKeySigner,
    transports::Transport,
};
use foundry_contracts::sckeystore::ScKeystore::{self, KeyPackage, ScKeystoreInstance};
use mls_crypto::openmls_provider::MlsCryptoProvider;
use openmls::test_utils::OpenMlsCryptoProvider;
use openmls::{
    key_packages::KeyPackageIn,
    prelude::{KeyPackage as mlsKeyPackage, TlsDeserializeTrait, TlsSerializeTrait},
    versions::ProtocolVersion,
};
use std::str::FromStr;

use crate::UserInfo;
use crate::UserKeyPackages;
use crate::{KeyStoreError, SCKeyStoreService};

pub struct ScKeyStorage<T, P, N> {
    instance: ScKeystoreInstance<T, P, N>,
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
        }
    }
}

impl<T: Transport + Clone, P: Provider<T, N>, N: Network> SCKeyStoreService
    for &mut ScKeyStorage<T, P, N>
{
    async fn does_user_exist(&self, id: &[u8]) -> Result<bool, KeyStoreError> {
        let address = Address::from_slice(id);
        let res = self.instance.userExists(address).call().await?;
        Ok(res._0)
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
        if self
            .does_user_exist(ukp.get_id_from_kp().as_slice())
            .await?
        {
            return Err(KeyStoreError::AlreadyExistedUserError);
        }

        let mut kp_bytes: Vec<Bytes> = Vec::with_capacity(ukp.0.len());
        for kp in ukp.0 {
            let bytes = kp.1.tls_serialize_detached()?;
            kp_bytes.push(Bytes::copy_from_slice(bytes.as_slice()))
        }

        let kp: KeyPackage = KeyPackage::from((kp_bytes,));
        let add_user_binding = self.instance.addUser(Bytes::copy_from_slice(sign_pk), kp);
        let res = add_user_binding.send().await;

        match res {
            Ok(_) => Ok(()),
            Err(err) => Err(KeyStoreError::AlloyError(err)),
        }
    }

    async fn get_user(
        &self,
        id: &[u8],
        crypto: &MlsCryptoProvider,
    ) -> Result<UserInfo, KeyStoreError> {
        let address = Address::from_slice(id);
        let res = self.instance.getUser(address).call().await;

        let user = match res {
            Ok(user) => user,
            Err(err) => return Err(KeyStoreError::AlloyError(err)),
        };

        let mut user = UserInfo {
            id: id.to_vec(),
            key_packages: UserKeyPackages::default(),
            sign_pk: user._0.signaturePubKey.to_vec(),
        };

        let res = self.instance.getAllKeyPackagesForUser(address).call().await;

        let kps = match res {
            Ok(kp) => kp,
            Err(err) => return Err(KeyStoreError::AlloyError(err)),
        };

        for kp in kps._0 {
            let key_package_in = KeyPackageIn::tls_deserialize_bytes(&kp.data[0])?;
            let key_package = key_package_in.validate(crypto.crypto(), ProtocolVersion::Mls10)?;
            let kp = key_package.hash_ref(crypto.crypto())?.as_slice().to_vec();
            user.key_packages.0.push((kp, key_package));
        }

        Ok(user)
    }

    async fn add_user_kp(&mut self, id: &[u8], ukp: UserKeyPackages) -> Result<(), KeyStoreError> {
        if !self.does_user_exist(id).await? {
            return Err(KeyStoreError::UnknownUserError);
        }

        let mut kp_bytes: Vec<Bytes> = Vec::with_capacity(ukp.0.len());
        for kp in ukp.0 {
            let bytes = kp.1.tls_serialize_detached()?;
            kp_bytes.push(Bytes::copy_from_slice(bytes.as_slice()))
        }

        let kp: KeyPackage = KeyPackage::from((kp_bytes,));

        let add_kp_binding = self.instance.addKeyPackage(kp);
        let res = add_kp_binding.send().await;

        match res {
            Ok(_) => Ok(()),
            Err(err) => Err(KeyStoreError::AlloyError(err)),
        }
    }

    async fn get_avaliable_user_kp(
        &mut self,
        id: &[u8],
        crypto: &MlsCryptoProvider,
    ) -> Result<mlsKeyPackage, KeyStoreError> {
        if !self.does_user_exist(id).await? {
            return Err(KeyStoreError::UnknownUserError);
        }
        let address = Address::from_slice(id);
        let add_kp_binding = self.instance.getAvailableKeyPackage(address);
        let res = add_kp_binding.call().await?;
        let key_package_in = KeyPackageIn::tls_deserialize_bytes(&res._0.data[0])?;
        let key_package = key_package_in.validate(crypto.crypto(), ProtocolVersion::Mls10)?;

        Ok(key_package)
    }
}

pub fn alice_addr_test() -> (Address, EthereumWallet) {
    let alice_address = Address::from_str("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap(); // anvil default key 0
    let signer = PrivateKeySigner::from_str(
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    )
    .unwrap();
    let wallet = EthereumWallet::from(signer);
    (alice_address, wallet)
}

pub fn bob_addr_test() -> (Address, EthereumWallet) {
    let bob_address = Address::from_str("0x70997970C51812dc3A010C7d01b50e0d17dc79C8").unwrap(); // anvil default key 0
    let signer = PrivateKeySigner::from_str(
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    )
    .unwrap();
    let wallet = EthereumWallet::from(signer);
    (bob_address, wallet)
}

pub fn carla_addr_test() -> (Address, EthereumWallet) {
    let carla_address = Address::from_str("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC").unwrap(); // anvil default key 0
    let signer = PrivateKeySigner::from_str(
        "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    )
    .unwrap();
    let wallet = EthereumWallet::from(signer);
    (carla_address, wallet)
}

mod test {
    use alloy::{primitives::Address, providers::ProviderBuilder};
    use std::{borrow::BorrowMut, collections::HashMap};

    use openmls::prelude::*;
    use openmls_basic_credential::SignatureKeyPair;

    use mls_crypto::openmls_provider::{MlsCryptoProvider, CIPHERSUITE};

    use crate::{sc_ks::*, UserKeyPackages};

    pub(crate) fn test_identity(
        address: Address,
        crypto: &MlsCryptoProvider,
    ) -> (UserKeyPackages, SignatureKeyPair) {
        let ciphersuite = CIPHERSUITE;
        let signature_keys = SignatureKeyPair::new(ciphersuite.signature_algorithm()).unwrap();
        let credential = Credential::new(address.to_vec(), CredentialType::Basic).unwrap();

        let credential_with_key = CredentialWithKey {
            credential,
            signature_key: signature_keys.to_public_vec().into(),
        };
        signature_keys.store(crypto.key_store()).unwrap();

        let key_package = mlsKeyPackage::builder()
            .build(
                CryptoConfig {
                    ciphersuite,
                    version: ProtocolVersion::default(),
                },
                crypto,
                &signature_keys,
                credential_with_key.clone(),
            )
            .unwrap();

        let kp = key_package.hash_ref(crypto.crypto()).unwrap();

        let mut kpgs = HashMap::from([(kp.as_slice().to_vec(), key_package)]);
        let ukp = UserKeyPackages(kpgs.drain().collect::<Vec<(Vec<u8>, mlsKeyPackage)>>());
        (ukp, signature_keys)
    }

    #[tokio::test]
    async fn test_sc_storage() {
        let crypto = MlsCryptoProvider::default();
        let storage_address =
            Address::from_str("0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512").unwrap();
        let (alice_address, wallet) = alice_addr_test();
        let provider = providers::ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_http(url::Url::from_str("http://localhost:8545").unwrap());

        let alice = test_identity(alice_address, &crypto);
        let mut binding = ScKeyStorage::new(provider, storage_address);
        let mut storage = binding.borrow_mut();

        let res = storage.add_user(alice.0, alice.1.public()).await;
        println!("Add user res: {:#?}", res);
        assert!(res.is_ok());

        let res = storage.get_user(alice_address.as_slice(), &crypto).await;
        println!("Get user res: {:#?}", res);
        assert!(res.is_ok());

        let res = storage.does_user_exist(alice_address.as_slice()).await;
        println!("User exist res: {:#?}", res);

        let res = storage
            .get_avaliable_user_kp(alice_address.as_slice(), &crypto)
            .await;
        println!("Get user kp: {:#?}", res);
        assert!(res.is_ok());
    }
}
