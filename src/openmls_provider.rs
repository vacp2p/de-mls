use openmls::prelude::*;
use openmls_rust_crypto::MemoryKeyStore;
use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsCryptoProvider;

pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

#[derive(Default)]
pub struct CryptoProvider {
    crypto: RustCrypto,
    key_storage: MemoryKeyStore,
}

impl OpenMlsCryptoProvider for CryptoProvider {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type KeyStoreProvider = MemoryKeyStore;

    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }

    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }

    fn key_store(&self) -> &Self::KeyStoreProvider {
        &self.key_storage
    }
}
