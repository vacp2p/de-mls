use alloy::hex;
use openmls::group::MlsGroup;
use openmls::prelude::Ciphersuite;
use openmls::prelude::KeyPackage;

use crate::error::{IdentityError, MlsServiceError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPackageBytes {
    bytes: Vec<u8>,
    identity: Vec<u8>,
}

impl KeyPackageBytes {
    pub fn new(bytes: Vec<u8>, identity: Vec<u8>) -> Self {
        Self { bytes, identity }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn identity_bytes(&self) -> &[u8] {
        &self.identity
    }

    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(&self.identity))
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub(crate) fn key_package_from_json(bytes: &[u8]) -> Result<KeyPackage, serde_json::Error> {
    serde_json::from_slice(bytes)
}

pub fn key_package_bytes_from_json(
    bytes: Vec<u8>,
) -> Result<(Vec<u8>, Vec<u8>), serde_json::Error> {
    let key_package: KeyPackage = serde_json::from_slice(&bytes)?;
    let identity = key_package
        .leaf_node()
        .credential()
        .serialized_content()
        .to_vec();
    Ok((bytes, identity))
}

#[derive(Debug)]
pub struct MlsGroupHandle {
    pub(crate) group: MlsGroup,
}

impl MlsGroupHandle {
    pub(crate) fn new(group: MlsGroup) -> Self {
        Self { group }
    }
}

#[derive(Clone, Debug)]
pub enum MlsGroupUpdate {
    AddMember(KeyPackageBytes),
    RemoveMember(Vec<u8>),
}

#[derive(Clone, Debug)]
pub enum MlsProcessResult {
    Application(Vec<u8>),
    LeaveGroup,
    Noop,
}

#[derive(Clone, Debug)]
pub struct BatchProposalsResult {
    pub proposals: Vec<Vec<u8>>,
    pub commit: Vec<u8>,
    pub welcome: Option<Vec<u8>>,
}

pub trait IdentityService: Send + Sync {
    const CIPHERSUITE: Ciphersuite;

    fn create_identity(&mut self, wallet: &[u8]) -> Result<(), IdentityError>;
    fn generate_key_package(&mut self) -> Result<KeyPackageBytes, IdentityError>;

    fn identity(&self) -> &[u8];
    fn identity_string(&self) -> String;

    fn is_key_package_exists(&self, kp_hash_ref: &[u8]) -> bool;
}

pub trait MlsGroupService: Send + Sync {
    fn create_group(&self, group_name: &str) -> Result<MlsGroupHandle, MlsServiceError>;
    fn join_group_from_invite(
        &self,
        invite_bytes: &[u8],
    ) -> Result<(MlsGroupHandle, Vec<u8>), MlsServiceError>;
    fn invite_new_member_hash_refs(
        &self,
        invite_bytes: &[u8],
    ) -> Result<Vec<Vec<u8>>, MlsServiceError>;
    fn group_members(&self, group: &MlsGroupHandle) -> Result<Vec<Vec<u8>>, MlsServiceError>;
    fn group_epoch(&self, group: &MlsGroupHandle) -> Result<u64, MlsServiceError>;
    fn process_inbound(
        &self,
        group: &mut MlsGroupHandle,
        msg_bytes: &[u8],
    ) -> Result<MlsProcessResult, MlsServiceError>;
    fn build_message(
        &self,
        group: &mut MlsGroupHandle,
        app_bytes: &[u8],
    ) -> Result<Vec<u8>, MlsServiceError>;
    fn create_batch_proposals(
        &self,
        group: &mut MlsGroupHandle,
        updates: &[MlsGroupUpdate],
    ) -> Result<BatchProposalsResult, MlsServiceError>;
}
