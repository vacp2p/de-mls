use alloy::primitives::Address;
use libsecp256k1::{PublicKey, SecretKey};
use openmls::prelude::KeyPackage;
use std::{fmt::Display, str::FromStr, sync::Arc};
use tokio::sync::Mutex;

use crate::{
    decrypt_message, error::MessageError, generate_keypair,
    protos::de_mls::messages::v1::GroupAnnouncement, sign_message,
};

#[derive(Clone, Debug)]
pub struct Steward {
    eth_pub: Arc<Mutex<PublicKey>>,
    eth_secr: Arc<Mutex<SecretKey>>,
    current_epoch_proposals: Arc<Mutex<Vec<GroupUpdateRequest>>>,
    voting_epoch_proposals: Arc<Mutex<Vec<GroupUpdateRequest>>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroupUpdateRequest {
    AddMember(Box<KeyPackage>),
    RemoveMember(String),
}

impl Display for GroupUpdateRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupUpdateRequest::AddMember(kp) => {
                let id = Address::from_slice(kp.leaf_node().credential().serialized_content());
                writeln!(f, "Add Member: {id:#?}")
            }
            GroupUpdateRequest::RemoveMember(id) => {
                let id = Address::from_str(id).unwrap();
                writeln!(f, "Remove Member: {id:#?}")
            }
        }
    }
}

impl Default for Steward {
    fn default() -> Self {
        Self::new()
    }
}

impl Steward {
    pub fn new() -> Self {
        let (public_key, private_key) = generate_keypair();
        Steward {
            eth_pub: Arc::new(Mutex::new(public_key)),
            eth_secr: Arc::new(Mutex::new(private_key)),
            current_epoch_proposals: Arc::new(Mutex::new(Vec::new())),
            voting_epoch_proposals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn refresh_key_pair(&mut self) {
        let (public_key, private_key) = generate_keypair();
        *self.eth_pub.lock().await = public_key;
        *self.eth_secr.lock().await = private_key;
    }

    pub async fn create_announcement(&self) -> GroupAnnouncement {
        let pub_key = self.eth_pub.lock().await;
        let sec_key = self.eth_secr.lock().await;
        let signature = sign_message(&pub_key.serialize_compressed(), &sec_key);
        GroupAnnouncement::new(pub_key.serialize_compressed().to_vec(), signature)
    }

    pub async fn decrypt_message(&self, message: Vec<u8>) -> Result<KeyPackage, MessageError> {
        let sec_key = self.eth_secr.lock().await;
        let msg: Vec<u8> = decrypt_message(&message, *sec_key)?;
        let key_package: KeyPackage = serde_json::from_slice(&msg)?;
        Ok(key_package)
    }

    /// Start a new steward epoch, moving current proposals to the epoch proposals map.
    pub async fn start_new_epoch(&mut self) {
        // Use a single atomic operation to move proposals between epochs
        let proposals = {
            let mut current = self.current_epoch_proposals.lock().await;
            current.drain(0..).collect::<Vec<_>>()
        };

        // Store proposals for this epoch (for voting and application)
        if !proposals.is_empty() {
            let mut voting = self.voting_epoch_proposals.lock().await;
            voting.extend(proposals);
        }
    }

    pub async fn get_current_epoch_proposals_count(&self) -> usize {
        self.current_epoch_proposals.lock().await.len()
    }

    /// Get proposals for the current epoch (for voting).
    pub async fn get_voting_epoch_proposals(&self) -> Vec<GroupUpdateRequest> {
        self.voting_epoch_proposals.lock().await.clone()
    }

    /// Get the count of proposals in the current epoch.
    pub async fn get_voting_epoch_proposals_count(&self) -> usize {
        self.voting_epoch_proposals.lock().await.len()
    }

    /// Apply proposals for the current epoch (called after successful voting).
    pub async fn empty_voting_epoch_proposals(&mut self) {
        self.voting_epoch_proposals.lock().await.clear();
    }

    /// Add a proposal to the current epoch
    pub async fn add_proposal(&mut self, proposal: GroupUpdateRequest) {
        self.current_epoch_proposals.lock().await.push(proposal);
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::signers::local::PrivateKeySigner;
    use mls_crypto::openmls_provider::{MlsProvider, CIPHERSUITE};
    use openmls::prelude::{BasicCredential, CredentialWithKey, KeyPackage};
    use openmls_basic_credential::SignatureKeyPair;

    use crate::steward::GroupUpdateRequest;
    #[tokio::test]
    async fn test_display_group_update_request() {
        let user_eth_priv_key =
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
        let signer =
            PrivateKeySigner::from_str(user_eth_priv_key).expect("Failed to create signer");
        let user_address = signer.address();

        let ciphersuite = CIPHERSUITE;
        let provider = MlsProvider::default();

        let credential = BasicCredential::new(user_address.as_slice().to_vec());
        let signer = SignatureKeyPair::new(ciphersuite.signature_algorithm())
            .expect("Error generating a signature key pair.");
        let credential_with_key = CredentialWithKey {
            credential: credential.into(),
            signature_key: signer.public().into(),
        };
        let key_package_bundle = KeyPackage::builder()
            .build(ciphersuite, &provider, &signer, credential_with_key)
            .expect("Error building key package bundle.");
        let key_package = key_package_bundle.key_package();

        let proposal_add_member = GroupUpdateRequest::AddMember(Box::new(key_package.clone()));
        assert_eq!(
            proposal_add_member.to_string(),
            "Add Member: 0x70997970c51812dc3a010c7d01b50e0d17dc79c8\n"
        );

        let proposal_remove_member = GroupUpdateRequest::RemoveMember(user_address.to_string());
        assert_eq!(
            proposal_remove_member.to_string(),
            "Remove Member: 0x70997970c51812dc3a010c7d01b50e0d17dc79c8\n"
        );
    }
}
