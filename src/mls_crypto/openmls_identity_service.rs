use openmls::prelude::{
    Ciphersuite, DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, ProcessedMessageContent,
    ProtocolMessage, StagedWelcome,
};
use openmls::{
    group::{GroupId, MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig},
    key_packages::KeyPackage,
};
use openmls_traits::OpenMlsProvider;

use crate::mls_crypto::{
    error::{IdentityError, MlsServiceError},
    identity::Identity,
    key_package_from_json,
    openmls_provider::{MlsProvider, CIPHERSUITE},
    BatchProposalsResult, IdentityService, KeyPackageBytes, MlsGroupHandle, MlsGroupService,
    MlsGroupUpdate, MlsProcessResult,
};

pub struct OpenMlsIdentityService {
    identity: Identity,
    mls_provider: MlsProvider,
}

impl OpenMlsIdentityService {
    pub fn new(user_wallet_address: &[u8]) -> Result<Self, IdentityError> {
        let crypto = MlsProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, user_wallet_address)?;
        Ok(OpenMlsIdentityService {
            identity: id,
            mls_provider: crypto,
        })
    }
}

impl IdentityService for OpenMlsIdentityService {
    const CIPHERSUITE: Ciphersuite = CIPHERSUITE;

    fn create_identity(&mut self, user_wallet_address: &[u8]) -> Result<(), IdentityError> {
        self.identity = Identity::new(CIPHERSUITE, &self.mls_provider, user_wallet_address)?;
        Ok(())
    }

    fn generate_key_package(&mut self) -> Result<KeyPackageBytes, IdentityError> {
        let key_package_bundle = KeyPackage::builder().build(
            Self::CIPHERSUITE,
            &self.mls_provider,
            self.identity.signer(),
            self.identity.credential_with_key().clone(),
        )?;
        let key_package = key_package_bundle.key_package();
        let kp = key_package.hash_ref(self.mls_provider.crypto())?;
        self.identity
            .kp
            .insert(kp.as_slice().to_vec(), key_package.clone());
        let bytes = serde_json::to_vec(key_package)?;
        let identity = key_package
            .leaf_node()
            .credential()
            .serialized_content()
            .to_vec();
        Ok(KeyPackageBytes::new(bytes, identity))
    }

    fn identity(&self) -> &[u8] {
        self.identity
            .credential_with_key
            .credential
            .serialized_content()
    }

    fn identity_string(&self) -> String {
        self.identity.identity_string()
    }

    fn is_key_package_exists(&self, kp_hash_ref: &[u8]) -> bool {
        self.identity.is_key_package_exists(kp_hash_ref)
    }
}

impl MlsGroupService for OpenMlsIdentityService {
    fn create_group(&self, group_name: &str) -> Result<MlsGroupHandle, MlsServiceError> {
        let group_config = MlsGroupCreateConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();
        let mls_group = MlsGroup::new_with_group_id(
            &self.mls_provider,
            self.identity.signer(),
            &group_config,
            GroupId::from_slice(group_name.as_bytes()),
            self.identity.credential_with_key().clone(),
        )?;
        Ok(MlsGroupHandle::new(mls_group))
    }

    fn join_group_from_invite(
        &self,
        invite_bytes: &[u8],
    ) -> Result<(MlsGroupHandle, Vec<u8>), MlsServiceError> {
        let group_config = MlsGroupJoinConfig::builder().build();
        let (mls_in, _) = MlsMessageIn::tls_deserialize_bytes(invite_bytes)?;
        let welcome = match mls_in.extract() {
            MlsMessageBodyIn::Welcome(welcome) => welcome,
            _ => return Err(MlsServiceError::UnexpectedMessageType),
        };
        let mls_group =
            StagedWelcome::new_from_welcome(&self.mls_provider, &group_config, welcome, None)?
                .into_group(&self.mls_provider)?;
        let group_id = mls_group.group_id().to_vec();
        Ok((MlsGroupHandle::new(mls_group), group_id))
    }

    fn invite_new_member_hash_refs(
        &self,
        invite_bytes: &[u8],
    ) -> Result<Vec<Vec<u8>>, MlsServiceError> {
        let (mls_in, _) = MlsMessageIn::tls_deserialize_bytes(invite_bytes)?;
        let welcome = match mls_in.extract() {
            MlsMessageBodyIn::Welcome(welcome) => welcome,
            _ => return Err(MlsServiceError::UnexpectedMessageType),
        };
        Ok(welcome
            .secrets()
            .iter()
            .map(|egs| egs.new_member().as_slice().to_vec())
            .collect())
    }

    fn group_members(&self, group: &MlsGroupHandle) -> Result<Vec<Vec<u8>>, MlsServiceError> {
        Ok(group
            .group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
            .collect())
    }

    fn group_epoch(&self, group: &MlsGroupHandle) -> Result<u64, MlsServiceError> {
        Ok(group.group.epoch().as_u64())
    }

    fn process_inbound(
        &self,
        group: &mut MlsGroupHandle,
        msg_bytes: &[u8],
    ) -> Result<MlsProcessResult, MlsServiceError> {
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(msg_bytes)?;
        let message: ProtocolMessage = mls_message_in.try_into_protocol_message()?;
        if message.group_id().as_slice() != group.group.group_id().as_slice() {
            return Ok(MlsProcessResult::Noop);
        }
        if message.epoch() < group.group.epoch() && message.epoch() == 0.into() {
            return Ok(MlsProcessResult::Noop);
        }

        let processed_message = group.group.process_message(&self.mls_provider, message)?;
        match processed_message.into_content() {
            ProcessedMessageContent::ApplicationMessage(application_message) => Ok(
                MlsProcessResult::Application(application_message.into_bytes()),
            ),
            ProcessedMessageContent::ProposalMessage(proposal_ptr) => {
                group.group.store_pending_proposal(
                    self.mls_provider.storage(),
                    proposal_ptr.as_ref().clone(),
                )?;
                Ok(MlsProcessResult::Noop)
            }
            ProcessedMessageContent::ExternalJoinProposalMessage(_) => Ok(MlsProcessResult::Noop),
            ProcessedMessageContent::StagedCommitMessage(commit_ptr) => {
                let remove_proposal = commit_ptr.self_removed();
                group
                    .group
                    .merge_staged_commit(&self.mls_provider, *commit_ptr)?;
                if remove_proposal {
                    if group.group.is_active() {
                        return Err(MlsServiceError::GroupStillActive);
                    }
                    Ok(MlsProcessResult::LeaveGroup)
                } else {
                    Ok(MlsProcessResult::Noop)
                }
            }
        }
    }

    fn build_message(
        &self,
        group: &mut MlsGroupHandle,
        app_bytes: &[u8],
    ) -> Result<Vec<u8>, MlsServiceError> {
        let message_out =
            group
                .group
                .create_message(&self.mls_provider, self.identity.signer(), app_bytes)?;
        Ok(message_out.to_bytes()?)
    }

    fn create_batch_proposals(
        &self,
        group: &mut MlsGroupHandle,
        updates: &[MlsGroupUpdate],
    ) -> Result<BatchProposalsResult, MlsServiceError> {
        let mut mls_proposals = Vec::new();
        for update in updates {
            match update {
                MlsGroupUpdate::AddMember(key_package_bytes) => {
                    let key_package = key_package_from_json(key_package_bytes.as_bytes())?;
                    let (mls_message_out, _proposal_ref) = group.group.propose_add_member(
                        &self.mls_provider,
                        self.identity.signer(),
                        &key_package,
                    )?;
                    mls_proposals.push(mls_message_out.to_bytes()?);
                }
                MlsGroupUpdate::RemoveMember(identity) => {
                    let member_index = group.group.members().find_map(|m| {
                        if m.credential.serialized_content() == identity {
                            Some(m.index)
                        } else {
                            None
                        }
                    });
                    if let Some(index) = member_index {
                        let (mls_message_out, _proposal_ref) = group.group.propose_remove_member(
                            &self.mls_provider,
                            self.identity.signer(),
                            index,
                        )?;
                        mls_proposals.push(mls_message_out.to_bytes()?);
                    }
                }
            }
        }

        let (out_messages, welcome, _group_info) = group
            .group
            .commit_to_pending_proposals(&self.mls_provider, self.identity.signer())?;
        group.group.merge_pending_commit(&self.mls_provider)?;

        let welcome_bytes = match welcome {
            Some(welcome) => Some(welcome.to_bytes()?),
            None => None,
        };

        Ok(BatchProposalsResult {
            proposals: mls_proposals,
            commit: out_messages.to_bytes()?,
            welcome: welcome_bytes,
        })
    }
}
