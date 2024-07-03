use std::{borrow::BorrowMut, collections::HashMap, str::FromStr};

use alloy::{
    network::{EthereumWallet, Network, TxSigner},
    primitives::{bytes::Buf, Address, Bytes},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    transports::Transport,
};
use p256::ecdsa::SigningKey;

// use alloy_contract;
use foundry_contracts::sckeystore::ScKeystore::{self, KeyPackage, ScKeystoreInstance};
use openmls::prelude::{KeyPackage as mlsKeyPackage, TlsSerializeTrait};
use openmls::{prelude::*, test_utils::OpenMlsRustCrypto};
use openmls_basic_credential::SignatureKeyPair;
use url::Url;

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
        let kp_bytes: Vec<Bytes> = ukp
            .0
            .iter()
            .map(|kp| Bytes::copy_from_slice(kp.tls_serialize_detached().unwrap().as_slice()))
            .collect();

        let kp: KeyPackage = KeyPackage::from((kp_bytes,));
        let res = self
            .instance
            .addUser(Bytes::copy_from_slice(sign_pk), kp)
            .call()
            .await;

        match res {
            Ok(_) => Ok(()),
            Err(err) => Err(KeyStoreError::AlloyError(err)),
        }
    }

    async fn get_user(&self, id: &[u8]) -> Result<UserInfo, KeyStoreError> {
        let address = Address::from_slice(id);
        let res = self.instance.getUser(address).call().await;

        let user = match res {
            Ok(user) => user,
            Err(err) => return Err(KeyStoreError::AlloyError(err)),
        };

        let u = user._0;
        println!("{:#?}", u.signaturePubKey);
        Ok(UserInfo::default())
    }

    async fn add_user_kp(&mut self, id: &[u8], ukp: UserKeyPackages) -> Result<(), KeyStoreError> {
        todo!()
    }

    async fn get_avaliable_user_kp(&mut self, id: &[u8]) -> Result<mlsKeyPackage, KeyStoreError> {
        todo!()
    }
}

fn test_identity() -> (UserKeyPackages, SignatureKeyPair, Address) {
    let ciphersuite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
    let crypto = OpenMlsRustCrypto::default();
    let signature_keys = SignatureKeyPair::new(ciphersuite.signature_algorithm()).unwrap();
    let addr = Address::ZERO;
    let credential = Credential::new(addr.to_vec(), CredentialType::Basic).unwrap();

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
            &crypto,
            &signature_keys,
            credential_with_key.clone(),
        )
        .unwrap();

    let kp = key_package.hash_ref(crypto.crypto()).unwrap();

    let mut kpgs = HashMap::from([(kp.as_slice().to_vec(), key_package)]);
    let ukp = UserKeyPackages(kpgs.drain().collect::<Vec<(Vec<u8>, mlsKeyPackage)>>());
    (ukp, signature_keys, addr)
}

#[tokio::test]
async fn test_sc_storage() {
    let res = Address::from_str("0x5FC8d32690cc91D4c39d9d3abcBD16989F875707");
    let alice_address = Address::from_str("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap(); // anvil default key 0
    let address = res.unwrap();
    assert!(res.is_ok());
    let signer = PrivateKeySigner::from_str(
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    )
    .unwrap();
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .on_http(Url::from_str("http://localhost:8545").unwrap());
    let alice = test_identity();
    let mut binding = ScKeyStorage::new(provider, address);
    let mut storage = binding.borrow_mut();

    let res = storage.add_user(alice.0, alice.1.public()).await;
    println!("res: {:#?}", res);

    let res = storage.get_user(alice_address.as_slice()).await;
    println!("res: {:#?}", res);

    let res = storage.does_user_exist(alice_address.as_slice()).await;
    println!("res: {:#?}", res);
}
