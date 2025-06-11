use libsecp256k1::{PublicKey, SecretKey};
use openmls::prelude::{hash_ref::ProposalRef, *};

use crate::{protos::messages::v1::GroupAnnouncement, *};

#[derive(Clone, Debug)]
pub struct GroupAdmin {
    eth_pub: PublicKey,
    eth_secr: SecretKey,
    incoming_key_packages: Arc<Mutex<Vec<KeyPackage>>>,
    pending_proposals: Arc<Mutex<Vec<ProposalRef>>>,
}

impl Default for GroupAdmin {
    fn default() -> Self {
        Self::new_admin()
    }
}

impl GroupAdmin {
    pub fn new_admin() -> Self {
        let (public_key, private_key) = generate_keypair();
        GroupAdmin {
            eth_pub: public_key,
            eth_secr: private_key,
            incoming_key_packages: Arc::new(Mutex::new(Vec::new())),
            pending_proposals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn refresh_key_pair(&mut self) {
        let (public_key, private_key) = generate_keypair();
        self.eth_pub = public_key;
        self.eth_secr = private_key;
    }

    pub fn create_admin_announcement(&self) -> GroupAnnouncement {
        let signature = sign_message(&self.eth_pub.serialize_compressed(), &self.eth_secr);
        GroupAnnouncement::new(self.eth_pub.serialize_compressed().to_vec(), signature)
    }

    pub fn decrypt_message(&self, message: Vec<u8>) -> Result<KeyPackage, MessageError> {
        let msg: Vec<u8> = decrypt_message(&message, self.eth_secr)?;
        let key_package: KeyPackage = serde_json::from_slice(&msg)?;
        Ok(key_package)
    }

    pub fn add_incoming_key_package(&mut self, key_package: KeyPackage) {
        self.incoming_key_packages.lock().unwrap().push(key_package)
    }

    pub fn drain_processed_key_packages(&mut self) -> Vec<KeyPackage> {
        self.incoming_key_packages
            .lock()
            .unwrap()
            .drain(0..)
            .collect()
    }

    pub fn add_pending_proposal(&mut self, proposal: ProposalRef) {
        self.pending_proposals.lock().unwrap().push(proposal);
    }

    pub fn get_pending_proposals(&self) -> Vec<ProposalRef> {
        self.pending_proposals.lock().unwrap().clone()
    }

    pub fn remove_pending_proposal(&mut self, proposal: ProposalRef) {
        self.pending_proposals
            .lock()
            .unwrap()
            .retain(|p| p != &proposal);
    }

    pub fn drain_pending_proposals(&mut self) -> Vec<ProposalRef> {
        self.pending_proposals.lock().unwrap().drain(0..).collect()
    }
}
