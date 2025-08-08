use libsecp256k1::{PublicKey, SecretKey};
use openmls::prelude::KeyPackage;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{protos::messages::v1::GroupAnnouncement, *};

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
    RemoveMember(Vec<u8>),
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
        // TODO: replace json in encryption and decryption
        let key_package: KeyPackage = serde_json::from_slice(&msg)?;
        Ok(key_package)
    }

    /// Start a new steward epoch, moving current proposals to the epoch proposals map and incrementing the epoch.
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
