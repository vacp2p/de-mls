//! [`MlsService`] — DE-MLS's per-conversation MLS engine, backed by OpenMLS.
//!
//! Wraps one `MlsGroup` and exposes just the MLS operations the conversation
//! needs: seeding or joining the group, the commit pipeline (build → stage →
//! merge/discard), encrypting and decrypting traffic, and membership queries.
//! Results cross the boundary as the de-mls byte types in [`crate::mls_crypto`],
//! so the rest of the library speaks de-mls types, not OpenMLS ones.
//!
//! Provider, signer, and credential arrive by reference per call — one provider
//! can back every conversation. The service keeps only the group and a
//! staged-commit slot; the integrator owns identity, keys, and storage.

use std::collections::HashSet;
use std::error::Error as StdError;

use openmls::credentials::CredentialWithKey;
use openmls::group::{
    GroupId, MlsGroup, MlsGroupCreateConfigBuilder, MlsGroupJoinConfigBuilder, StagedCommit,
    StagedWelcome, WelcomeError,
};
use openmls::key_packages::{KeyPackage, KeyPackageIn};
use openmls::prelude::{
    ContentType, DeserializeBytes, LeafNodeIndex, MlsMessageBodyIn, MlsMessageIn,
    ProcessedMessageContent, ProtocolMessage, ProtocolVersion, Sender,
};
use openmls_traits::storage::StorageProvider;
use openmls_traits::{OpenMlsProvider, signatures::Signer};
use prost::Message;

use crate::mls_crypto::member_id::{MemberId, leaf_index_of, member_id_of};
use crate::{
    mls_crypto::{
        CommitArtifacts, DecryptedMessage, MembershipDelta, MlsCommitInput, MlsError,
        MlsMessageKind, MlsProposalOutput, PAST_EPOCH_WINDOW, StagedCandidateResult, pin, pin_join,
    },
    protos::de_mls::messages::v1::AppMessage,
};

/// DE-MLS's MLS engine for one conversation: a single `MlsGroup` plus the
/// staged-commit slot for the inbound stage→merge/discard pipeline.
pub struct MlsService {
    conversation_id: String,
    group: MlsGroup,
    pending_staged_commit: Option<StagedCommit>,
    /// The epoch this member entered the group at; the decrypt gate floors
    /// here (earlier epochs used keys we never held). In-memory only — a
    /// future reload-from-storage path must re-derive or persist it.
    first_epoch: u64,
}

/// Parse and cryptographically validate a key package, returning the validated
/// [`KeyPackage`]. This is the gate a joiner passes to enter the group; call it
/// before proposing an Add so a malformed or invalid key package fails at the
/// caller rather than only when a steward commits it.
pub fn validate_key_package<Pr>(
    provider: &Pr,
    key_package_bytes: &[u8],
) -> Result<KeyPackage, MlsError>
where
    Pr: OpenMlsProvider,
{
    let (kp_in, _) =
        KeyPackageIn::tls_deserialize_bytes(key_package_bytes).map_err(MlsError::KeyPackageTls)?;
    kp_in
        .validate(provider.crypto(), ProtocolVersion::Mls10)
        .map_err(MlsError::storage)
}

/// The leaf signature key of a key package — the id a joiner is tracked by
/// until the Add commit gives it a leaf. Reads it without validating the key
/// package; validation happens at commit. Keyed on the signature key rather
/// than the credential because MLS requires signature keys to be unique within
/// a group, while a credential is integrator-supplied and may repeat across
/// members.
pub fn signature_key_of_key_package(key_package_bytes: &[u8]) -> Result<Vec<u8>, MlsError> {
    let (kp_in, _) =
        KeyPackageIn::tls_deserialize_bytes(key_package_bytes).map_err(MlsError::KeyPackageTls)?;
    Ok(kp_in
        .unverified_credential()
        .signature_key
        .as_slice()
        .to_vec())
}

impl MlsService {
    // ══════════════════════════════════════════════════════════
    // Construction & teardown
    // ══════════════════════════════════════════════════════════

    /// Create a fresh group as its sole initial member ("creator"). The leaf is
    /// seeded straight from `credential` and `ciphersuite` — no key package,
    /// since key packages are how *joiners* are added. `provider` is written
    /// into but not retained.
    ///
    /// `group_config` carries the integrator's ciphersuite, capabilities, and
    /// extensions. de-mls stamps its own reliability settings on top before
    /// building, so every group runs the same ones.
    pub fn new_as_creator<Pr>(
        conversation_id: String,
        provider: &Pr,
        credential: CredentialWithKey,
        group_config: MlsGroupCreateConfigBuilder,
        signer: &impl Signer,
    ) -> Result<Self, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let group = MlsGroup::new_with_group_id(
            provider,
            signer,
            &pin(group_config),
            GroupId::from_slice(conversation_id.as_bytes()),
            credential,
        )?;

        Ok(Self {
            conversation_id,
            first_epoch: group.epoch().as_u64(),
            group,
            pending_staged_commit: None,
        })
    }

    /// Join a group from a welcome. `provider` must hold the joiner's
    /// key-package private keys from the earlier key-package build, and is read
    /// from but not retained. Returns `Ok(None)` when the welcome doesn't
    /// address one of our key packages — the "not for us" branch, not an error.
    ///
    /// `join_config` mirrors the create path's builder: de-mls stamps its own
    /// reliability settings on top before building, so a joiner runs the same
    /// ones as the creator.
    pub fn new_from_welcome<Pr>(
        provider: &Pr,
        welcome_bytes: &[u8],
        join_config: MlsGroupJoinConfigBuilder,
    ) -> Result<Option<Self>, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Ok(None),
        };

        let config = pin_join(join_config);
        let staged = match StagedWelcome::new_from_welcome(provider, &config, welcome, None) {
            Ok(staged) => staged,
            Err(WelcomeError::NoMatchingKeyPackage | WelcomeError::JoinerSecretNotFound) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let group = staged.into_group(provider)?;

        let conversation_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();
        Ok(Some(Self {
            conversation_id,
            first_epoch: group.epoch().as_u64(),
            group,
            pending_staged_commit: None,
        }))
    }

    /// Tear down all local MLS state for this conversation. Idempotent, so
    /// repeated leave / cleanup is safe.
    pub fn delete<Pr>(&mut self, provider: &Pr) -> Result<(), MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.group
            .delete(provider.storage())
            .map_err(MlsError::storage)
    }

    // ══════════════════════════════════════════════════════════
    // Queries
    // ══════════════════════════════════════════════════════════

    /// The conversation id this group is scoped to.
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// Provides read-only access to the underlying MLS group using the OpenMLS API.
    pub fn group(&self) -> &MlsGroup {
        &self.group
    }

    /// Returns the `member_id` of all current members.
    pub fn members(&self) -> Result<Vec<Vec<u8>>, MlsError> {
        Ok(self
            .group
            .members()
            .map(|m| member_id_of(m.index))
            .collect())
    }

    /// Whether `member_id` a current member in the MLS.
    pub fn is_member(&self, member_id: &[u8]) -> bool {
        leaf_index_of(member_id).is_some_and(|index| self.group.member_at(index).is_some())
    }

    // ── Leaf-index identity ───────────────────────────────────────

    /// The leaf a [`MemberId`] currently resolves to, or `None` if the handle is
    /// stale — its member left, or the leaf was reused (signature-key mismatch).
    pub fn leaf_of(&self, id: &MemberId) -> Option<LeafNodeIndex> {
        let member = self.group.member_at(id.leaf())?;
        (member.signature_key.as_slice() == id.tag()).then_some(id.leaf())
    }

    /// The [`MemberId`] handle for the member at `member_id` (leaf bytes), or
    /// `None` if no current member sits there.
    pub fn member_id_at(&self, member_id: &[u8]) -> Option<MemberId> {
        let index = leaf_index_of(member_id)?;
        self.group.member_at(index).map(|m| MemberId::from(&m))
    }

    // ══════════════════════════════════════════════════════════
    // Outbound commit: build our own, then resolve
    // ══════════════════════════════════════════════════════════

    /// Build a commit candidate from a batch of membership changes, returning
    /// the wire bytes (proposals, commit, and a welcome if anyone is added) for
    /// the steward to broadcast.
    ///
    /// The candidate is staged locally, not applied — another steward's commit
    /// may win the commit round. Resolve it before the next MLS operation:
    /// [`merge_own_commit`](Self::merge_own_commit) if it wins,
    /// [`discard_own_commit`](Self::discard_own_commit) otherwise; a leftover
    /// pending commit blocks the next commit or message.
    pub fn create_commit_candidate<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        updates: &[MlsCommitInput],
    ) -> Result<CommitArtifacts, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let group = &mut self.group;
        let mut mls_proposals = Vec::new();

        for update in updates {
            match update {
                MlsCommitInput::Add(key_package_bytes) => {
                    let kp = validate_key_package(provider, key_package_bytes)?;
                    let (mls_message_out, _) = group.propose_add_member(provider, signer, &kp)?;
                    mls_proposals.push(mls_message_out.to_bytes()?);
                }
                MlsCommitInput::Remove(member_id) => {
                    if let Some(index) = leaf_index_of(member_id)
                        && group.member_at(index).is_some()
                    {
                        let (mls_message_out, _) =
                            group.propose_remove_member(provider, signer, index)?;
                        mls_proposals.push(mls_message_out.to_bytes()?);
                    }
                }
            }
        }

        let (commit_msg, welcome, _group_info) =
            group.commit_to_pending_proposals(provider, signer)?;

        let welcome_bytes = match welcome {
            Some(w) => Some(w.to_bytes()?),
            None => None,
        };

        Ok(CommitArtifacts {
            proposals: mls_proposals,
            commit: commit_msg.to_bytes()?,
            welcome: welcome_bytes,
        })
    }

    /// Apply our staged commit, advancing the epoch — call once our candidate
    /// has won the commit round. Returns the membership change it applied.
    pub fn merge_own_commit<Pr>(&mut self, provider: &Pr) -> Result<MembershipDelta, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let removed = match self.group.pending_commit() {
            Some(staged) => self.removed_members_of(staged),
            None => Vec::new(),
        };
        let before = self.populated_leaves();
        self.group.merge_pending_commit(provider)?;
        Ok(self.delta(before, removed))
    }

    /// The members a commit's Remove proposals vacate, as [`MemberId`] handles.
    /// Called before the commit merges — the tree still seats them, so each
    /// handle captures the departing member's leaf and signature key.
    fn removed_members_of(&self, staged: &StagedCommit) -> Vec<MemberId> {
        staged
            .remove_proposals()
            .filter_map(|r| {
                let leaf = r.remove_proposal().removed();
                self.group.member_at(leaf).map(|m| MemberId::from(&m))
            })
            .collect()
    }

    /// The leaf indices this group currently seats.
    fn populated_leaves(&self) -> HashSet<u32> {
        self.group.members().map(|m| m.index.u32()).collect()
    }

    /// Returns who was added or removed by the latest commit, based on the Remove
    /// proposals and the updated tree. New or recycled leaves show up as added;
    /// removed leaves are from this commit’s Remove proposals.
    fn delta(&self, before: HashSet<u32>, removed: Vec<MemberId>) -> MembershipDelta {
        let removed_set: HashSet<u32> = removed.iter().map(|id| id.leaf().u32()).collect();
        let added = self
            .group
            .members()
            .filter(|m| !before.contains(&m.index.u32()) || removed_set.contains(&m.index.u32()))
            .collect();
        MembershipDelta { added, removed }
    }

    /// Drop our staged commit and the pending proposals it carried — call when
    /// the candidate lost selection or we're rolling back.
    pub fn discard_own_commit<Pr>(&mut self, provider: &Pr) -> Result<(), MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.group
            .clear_pending_commit(provider.storage())
            .map_err(MlsError::storage)?;
        self.group
            .clear_pending_proposals(provider.storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Inbound commit: stage a peer's, then resolve
    // ══════════════════════════════════════════════════════════

    /// Stage a peer's commit candidate: each proposal is processed as pending,
    /// then the commit against them, leaving a staged commit held internally —
    /// not applied.
    ///
    /// The caller validates the result (sender, actions vs. the voted-approved
    /// set) and follows up with
    /// [`merge_staged_commit`](Self::merge_staged_commit) to advance the epoch
    /// or [`discard_staged_commit`](Self::discard_staged_commit) to roll back.
    /// See [`StagedCandidateResult`] for the outcomes; a benign
    /// [`Aborted`](StagedCandidateResult::Aborted) still needs a
    /// `discard_staged_commit` to clear partial state before the next candidate.
    pub fn stage_remote_commit<Pr>(
        &mut self,
        provider: &Pr,
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let group = &mut self.group;
        let conversation_id = &self.conversation_id;

        // ── Stage every proposal, collecting senders ──
        let mut proposal_senders: Vec<Vec<u8>> = Vec::with_capacity(proposals.len());
        for (i, proposal_bytes) in proposals.iter().enumerate() {
            let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(proposal_bytes)?;
            let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;
            let processed = group.process_message(provider, protocol_message)?;
            let sender = match processed.sender() {
                Sender::Member(idx) => member_id_of(*idx),
                _ => return Ok(StagedCandidateResult::Aborted),
            };
            match processed.into_content() {
                ProcessedMessageContent::ProposalMessage(proposal) => {
                    group
                        .store_pending_proposal(provider.storage(), proposal.as_ref().clone())
                        .map_err(MlsError::storage)?;
                    proposal_senders.push(sender);
                }
                _ => {
                    tracing::debug!(
                        group = %conversation_id,
                        index = i,
                        "stage_remote_commit: non-proposal in proposal slot",
                    );
                    return Ok(StagedCandidateResult::Aborted);
                }
            }
        }

        // ── Stage the commit ──
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(commit_bytes)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            tracing::debug!(
                "stage_remote_commit: ignoring commit for wrong group ID (expected {})",
                conversation_id,
            );
            return Ok(StagedCandidateResult::Aborted);
        }
        if protocol_message.epoch() < group.epoch() {
            tracing::debug!(
                "stage_remote_commit: ignoring stale commit from epoch {} (current: {})",
                protocol_message.epoch().as_u64(),
                group.epoch().as_u64(),
            );
            return Ok(StagedCandidateResult::Aborted);
        }

        let processed = group.process_message(provider, protocol_message)?;
        let commit_sender = match processed.sender() {
            Sender::Member(idx) => member_id_of(*idx),
            _ => return Ok(StagedCandidateResult::Aborted),
        };

        let outcome = match processed.into_content() {
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                let self_removed = staged.self_removed();
                let mut actions = Vec::new();
                // An Add targets a not-yet-member, identified by its leaf signature key.
                for add in staged.add_proposals() {
                    let id = add
                        .add_proposal()
                        .key_package()
                        .leaf_node()
                        .signature_key()
                        .as_slice()
                        .to_vec();
                    actions.push(MlsProposalOutput::Add(id));
                }
                // A Remove targets an existing member by its leaf index.
                for remove in staged.remove_proposals() {
                    let removed_index = remove.remove_proposal().removed();
                    actions.push(MlsProposalOutput::Remove(member_id_of(removed_index)));
                }
                Some((commit_sender, self_removed, actions, *staged))
            }
            _ => {
                tracing::debug!(
                    "stage_remote_commit: ignoring non-commit message for group {}",
                    conversation_id,
                );
                None
            }
        };

        match outcome {
            Some((commit_sender, self_removed, actions, staged)) => {
                // de-mls invariant: every bundled proposal must come from the
                // committer. MLS allows reference-by-id of others' proposals;
                // we don't.
                if proposal_senders.iter().any(|s| s != &commit_sender) {
                    return Ok(StagedCandidateResult::BundleSenderMismatch { commit_sender });
                }
                self.pending_staged_commit = Some(staged);
                Ok(StagedCandidateResult::Staged {
                    commit_sender,
                    self_removed,
                    actions,
                })
            }
            None => Ok(StagedCandidateResult::Aborted),
        }
    }

    /// Apply the staged peer commit, advancing the epoch. Errors if nothing is
    /// staged. Returns the membership change it applied.
    pub fn merge_staged_commit<Pr>(&mut self, provider: &Pr) -> Result<MembershipDelta, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let staged = self
            .pending_staged_commit
            .take()
            .ok_or_else(|| MlsError::NoPendingStagedCommit(self.conversation_id.clone()))?;
        let removed = self.removed_members_of(&staged);
        let before = self.populated_leaves();
        self.group.merge_staged_commit(provider, staged)?;
        Ok(self.delta(before, removed))
    }

    /// Drop the staged peer commit and the pending proposals it staged on top
    /// of.
    pub fn discard_staged_commit<Pr>(&mut self, provider: &Pr) -> Result<(), MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.pending_staged_commit = None;
        self.group
            .clear_pending_proposals(provider.storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Application messages
    // ══════════════════════════════════════════════════════════

    /// Encode and encrypt an [`AppMessage`], returning the raw payload bytes —
    /// the convenience path most senders use.
    pub fn build_message<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        app_msg: &AppMessage,
    ) -> Result<Vec<u8>, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let message = self
            .group
            .create_message(provider, signer, &app_msg.encode_to_vec())?;
        Ok(message.to_bytes()?)
    }

    /// Strict decrypt: accept only application messages, ignoring everything
    /// else (proposals and commits included). Guards the application subtopic
    /// against MLS-state pollution from peers that misroute control messages.
    ///
    /// Accepts the current epoch and up to [`PAST_EPOCH_WINDOW`] behind it,
    /// floored at our join epoch.
    ///
    /// Returns the [`DecryptedMessage`], or `None` when the message can't be
    /// taken as one (a proposal, commit, wrong group, or an epoch outside the
    /// readable window is dropped, not an error).
    pub fn decrypt_application_only<Pr>(
        &mut self,
        provider: &Pr,
        ciphertext: &[u8],
    ) -> Result<Option<DecryptedMessage>, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let group = &mut self.group;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            return Ok(None);
        }

        // Drop epochs we hold no key for before OpenMLS raises a hard error:
        // future (joiner sent at N+1 before we merged), older than the
        // retained window, or before we joined. None is the sender's fault.
        let group_epoch = group.epoch().as_u64();
        let message_epoch = protocol_message.epoch().as_u64();
        if message_epoch > group_epoch
            || message_epoch < self.first_epoch
            || group_epoch - message_epoch > PAST_EPOCH_WINDOW as u64
        {
            return Ok(None);
        }

        // Reject commits/proposals before process_message to avoid MLS errors
        // (e.g. MissingProposal when commit's proposals aren't stored).
        match protocol_message.content_type() {
            ContentType::Commit | ContentType::Proposal => {
                return Ok(None);
            }
            ContentType::Application => {}
        }

        let processed = group.process_message(provider, protocol_message)?;

        let member = match processed.sender() {
            Sender::Member(leaf_index) => group
                .member_at(*leaf_index)
                .ok_or(MlsError::UnknownLeafIndex(leaf_index.u32()))?,
            _ => return Ok(None),
        };

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(Some(DecryptedMessage {
                payload: app.into_bytes(),
                member,
            })),
            _ => Ok(None),
        }
    }

    /// Peek a wire message's outer kind without processing or signature-checking
    /// it — a cheap pre-dispatch lane check (e.g. "proposal or commit?").
    pub fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind, MlsError> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(message_bytes)?;
        let protocol = match mls_message.extract() {
            MlsMessageBodyIn::PrivateMessage(m) => ProtocolMessage::PrivateMessage(m),
            MlsMessageBodyIn::PublicMessage(m) => ProtocolMessage::PublicMessage(Box::new(m)),
            _ => return Ok(MlsMessageKind::Other),
        };

        let kind = match protocol.content_type() {
            ContentType::Proposal => MlsMessageKind::Proposal,
            ContentType::Commit => MlsMessageKind::Commit,
            ContentType::Application => MlsMessageKind::Other,
        };
        Ok(kind)
    }
}

#[cfg(test)]
mod service_tests {
    use openmls::group::MlsGroupJoinConfig;

    use super::*;
    use crate::test_fixtures::{TestProvider, make_creator_mls, member_key_package};

    fn fresh_key_package(provider: &TestProvider, member_id: &[u8]) -> Vec<u8> {
        member_key_package(provider, member_id).0
    }

    /// A *failed* remote-commit stage must leave our own pending commit intact.
    /// OpenMLS only clears the own pending commit when a remote is *successfully*
    /// processed, so a remote that never processes (malformed / stale / wrong
    /// group → `Abort`) does not touch it. This is the evidence that the
    /// lost-own-commit lag is caused by de-mls's explicit pre-discard in
    /// `apply_in_priority_order`, not by the act of staging a failing remote.
    #[test]
    fn failed_remote_stage_preserves_own_pending_commit() {
        let (mut mls, provider, signer) = make_creator_mls(b"alice");

        // Build a real own pending commit (Add bob) — what a backup steward holds
        // when the epoch steward's candidate arrives.
        let bob_kp = fresh_key_package(&provider, b"bob");
        mls.create_commit_candidate(&provider, &signer, &[MlsCommitInput::Add(bob_kp)])
            .expect("own commit candidate");
        let epoch_before = mls.group().epoch().as_u64();

        // Stage an invalid remote commit (garbage). It must not apply, and must
        // not disturb our own pending commit.
        let staged = mls.stage_remote_commit(&provider, &[], &[0xFFu8; 64]);
        assert!(
            matches!(staged, Ok(StagedCandidateResult::Aborted) | Err(_)),
            "garbage remote commit must not stage"
        );

        // Our own pending commit should still merge cleanly → it survived.
        mls.merge_own_commit(&provider)
            .expect("own pending commit survived a failed remote stage");
        assert_eq!(
            mls.group().epoch().as_u64(),
            epoch_before + 1,
            "own commit applied after the failed remote, epoch advanced"
        );
    }

    /// The discriminating experiment, and the key enabler for the reorder fix:
    /// staging a *valid* remote commit while our own commit is pending succeeds,
    /// and crucially does NOT clear our own — OpenMLS clears the own pending
    /// commit only when a remote is *merged* (applied), not when it's staged.
    /// So we can stage + validate any remote, and if we reject it
    /// (`discard_staged_commit`), our own commit is still there to apply. The
    /// explicit `discard_own_commit` pre-discard in `apply_in_priority_order` is
    /// therefore unnecessary and is the sole cause of the lost-own-commit lag.
    #[test]
    fn own_commit_survives_staging_then_rejecting_a_valid_remote() {
        let (mut alice, provider_a, signer_a) = make_creator_mls(b"alice");
        let provider_b = TestProvider::default();
        let (bob_kp, bob_signer) = member_key_package(&provider_b, b"bob");

        // Alice adds bob and merges → alice + bob synced at the same epoch.
        let artifacts = alice
            .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(bob_kp)])
            .expect("add bob");
        alice.merge_own_commit(&provider_a).unwrap();
        let welcome = artifacts.welcome.expect("welcome for bob");
        let mut bob =
            MlsService::new_from_welcome(&provider_b, &welcome, MlsGroupJoinConfig::builder())
                .expect("open welcome")
                .expect("welcome addressed to bob");
        let synced_epoch = alice.group().epoch().as_u64();
        assert_eq!(synced_epoch, bob.group().epoch().as_u64());

        // Bob builds a valid commit (add carol) — a genuine remote candidate.
        let carol_kp = fresh_key_package(&provider_b, b"carol");
        let bob_artifacts = bob
            .create_commit_candidate(&provider_b, &bob_signer, &[MlsCommitInput::Add(carol_kp)])
            .expect("bob commit");

        // Alice builds her OWN commit (add dave) → alice now has a pending commit.
        let dave_kp = fresh_key_package(&provider_a, b"dave");
        alice
            .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(dave_kp)])
            .expect("alice own commit");

        // Stage bob's valid remote commit while alice's own is still pending.
        let staged = alice
            .stage_remote_commit(&provider_a, &bob_artifacts.proposals, &bob_artifacts.commit)
            .expect("stage bob's commit");
        assert!(
            matches!(staged, StagedCandidateResult::Staged { .. }),
            "a valid remote stages even while our own commit is pending, got {staged:?}"
        );

        // Reject the remote (as the round would for a losing/invalid candidate)
        // — this must NOT take our own pending commit with it.
        alice.discard_staged_commit(&provider_a).unwrap();

        // Our own commit is still pending and applies cleanly.
        alice
            .merge_own_commit(&provider_a)
            .expect("own pending commit survived staging + rejecting a valid remote");
        assert_eq!(alice.group().epoch().as_u64(), synced_epoch + 1);
    }

    /// The surgical fix shape: in de-mls's split staging flow, merging a remote
    /// does NOT auto-clear our own pending commit (it lingers). So the reorder is
    /// "discard our own *right before* merging the winning remote" — which
    /// preserves our own across rejected remotes but cleans it up before applying
    /// a winner. This test pins that exact sequence: stage → discard own → merge
    /// remote → our own is gone and the remote applied.
    #[test]
    fn discard_own_then_merge_remote_applies_remote_and_clears_own() {
        let (mut alice, provider_a, signer_a) = make_creator_mls(b"alice");
        let provider_b = TestProvider::default();
        let (bob_kp, bob_signer) = member_key_package(&provider_b, b"bob");

        let artifacts = alice
            .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(bob_kp)])
            .expect("add bob");
        alice.merge_own_commit(&provider_a).unwrap();
        let welcome = artifacts.welcome.expect("welcome for bob");
        let mut bob =
            MlsService::new_from_welcome(&provider_b, &welcome, MlsGroupJoinConfig::builder())
                .expect("open welcome")
                .expect("welcome addressed to bob");
        let synced_epoch = alice.group().epoch().as_u64();

        // Bob builds a valid commit; alice builds her own → alice has a pending commit.
        let carol_kp = fresh_key_package(&provider_b, b"carol");
        let bob_artifacts = bob
            .create_commit_candidate(&provider_b, &bob_signer, &[MlsCommitInput::Add(carol_kp)])
            .expect("bob commit");
        let dave_kp = fresh_key_package(&provider_a, b"dave");
        alice
            .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(dave_kp)])
            .expect("alice own commit");

        // Stage bob's remote (own survives), then — the fix — discard our own
        // right before merging the winning remote.
        alice
            .stage_remote_commit(&provider_a, &bob_artifacts.proposals, &bob_artifacts.commit)
            .expect("stage bob's commit");
        alice
            .discard_own_commit(&provider_a)
            .expect("discard own before merging the winning remote");
        alice
            .merge_staged_commit(&provider_a)
            .expect("merge the remote after discarding own");
        assert_eq!(alice.group().epoch().as_u64(), synced_epoch + 1);

        // The remote applied (carol added) and our own was discarded, not
        // applied (dave absent).
        let members: Vec<_> = alice.group().members().collect();
        let has = |name: &[u8]| {
            members
                .iter()
                .any(|m| m.credential.serialized_content() == name)
        };
        assert!(has(b"carol"), "the remote commit (add carol) applied");
        assert!(
            !has(b"dave"),
            "our own commit (add dave) was discarded before the merge, not applied"
        );
    }

    /// Recycling: one commit removes the member at a leaf and adds a new one
    /// that refills it. The merge delta must list that leaf in BOTH `removed`
    /// and `added` — the delta reads the commit's own Remove proposals, so a
    /// leaf a Remove vacated and an Add refilled counts as both, and the reused
    /// leaf never carries the old member's per-leaf state (score) to the new one.
    #[test]
    fn merge_delta_flags_a_reused_leaf_as_removed_and_added() {
        let (mut alice, provider_a, signer_a) = make_creator_mls(b"alice");
        let joiners = TestProvider::default();

        // Seat alice(0), bob(1), carol(2) so bob's leaf is an interior slot.
        for name in [b"bob".as_slice(), b"carol".as_slice()] {
            let kp = fresh_key_package(&joiners, name);
            alice
                .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(kp)])
                .expect("add member");
            alice.merge_own_commit(&provider_a).expect("merge add");
        }
        let bob_leaf = LeafNodeIndex::new(1);
        assert!(
            alice
                .group()
                .members()
                .any(|m| m.index == bob_leaf && m.credential.serialized_content() == b"bob"),
            "bob is seated at leaf 1"
        );

        // One commit: remove bob (leaf 1) and add dave, who refills that leaf.
        let dave_kp = fresh_key_package(&joiners, b"dave");
        alice
            .create_commit_candidate(
                &provider_a,
                &signer_a,
                &[
                    MlsCommitInput::Remove(member_id_of(bob_leaf)),
                    MlsCommitInput::Add(dave_kp),
                ],
            )
            .expect("remove bob + add dave in one commit");
        let delta = alice
            .merge_own_commit(&provider_a)
            .expect("merge the recycling commit");

        // Dave now holds bob's old leaf; bob is gone.
        assert!(
            alice
                .group()
                .members()
                .any(|m| m.index == bob_leaf && m.credential.serialized_content() == b"dave"),
            "dave refilled leaf 1"
        );
        assert!(
            !alice
                .group()
                .members()
                .any(|m| m.credential.serialized_content() == b"bob"),
            "bob is no longer a member"
        );

        // The reused leaf is on BOTH sides of the delta — a pure index diff
        // (leaf 1 present before and after) would miss the swap and leak bob's
        // state onto dave.
        assert!(
            delta.removed.iter().any(|id| id.leaf() == bob_leaf),
            "reused leaf listed as removed; removed={:?}",
            delta
                .removed
                .iter()
                .map(|id| id.leaf().u32())
                .collect::<Vec<_>>()
        );
        assert!(
            delta
                .added
                .iter()
                .any(|m| m.index == bob_leaf && m.credential.serialized_content() == b"dave"),
            "reused leaf listed as added with the new member"
        );
    }

    /// The pinned settings exist for this: a message sent just before a commit
    /// lands still decrypts afterwards. At OpenMLS's `max_past_epochs` default
    /// of 0 it is lost, which is what strands a member that missed a commit.
    #[test]
    fn a_message_from_the_previous_epoch_still_decrypts() {
        use crate::protos::de_mls::messages::v1::{AppMessage, ConversationMessage, app_message};
        let chat = AppMessage {
            payload: Some(app_message::Payload::ConversationMessage(
                ConversationMessage {
                    message: "sent before the commit".into(),
                    ..Default::default()
                },
            )),
        };

        let (mut alice, provider_a, signer_a) = make_creator_mls(b"alice");
        let provider_b = TestProvider::default();
        let (bob_kp, _bob_signer) = member_key_package(&provider_b, b"bob");
        let artifacts = alice
            .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(bob_kp)])
            .expect("add bob");
        alice.merge_own_commit(&provider_a).unwrap();
        let mut bob = MlsService::new_from_welcome(
            &provider_b,
            &artifacts.welcome.unwrap(),
            MlsGroupJoinConfig::builder(),
        )
        .expect("open welcome")
        .expect("welcome addressed to bob");

        // In flight when the next commit lands.
        let straggler = alice
            .build_message(&provider_a, &signer_a, &chat)
            .expect("chat at the current epoch");

        // Both advance an epoch before it is delivered.
        let carol_kp = fresh_key_package(&provider_a, b"carol");
        let next = alice
            .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(carol_kp)])
            .expect("add carol");
        let staged = bob
            .stage_remote_commit(&provider_b, &next.proposals, &next.commit)
            .expect("stage");
        assert!(matches!(staged, StagedCandidateResult::Staged { .. }));
        bob.merge_staged_commit(&provider_b).expect("merge");
        alice.merge_own_commit(&provider_a).expect("merge own");

        let epoch = bob.group().epoch().as_u64();
        let decrypted = bob
            .decrypt_application_only(&provider_b, &straggler)
            .expect("a past-epoch message is readable inside the pinned window");
        assert!(
            decrypted.is_some(),
            "message from epoch {} dropped at epoch {epoch}",
            epoch - 1
        );
    }

    /// A pre-join straggler — sealed with epoch keys the joiner never held —
    /// is dropped, never surfaced as an error.
    #[test]
    fn a_message_from_before_our_join_is_dropped_not_raised() {
        use crate::protos::de_mls::messages::v1::{AppMessage, ConversationMessage, app_message};
        let chat = AppMessage {
            payload: Some(app_message::Payload::ConversationMessage(
                ConversationMessage {
                    message: "sent while bob was still outside".into(),
                    ..Default::default()
                },
            )),
        };

        let (mut alice, provider_a, signer_a) = make_creator_mls(b"alice");
        // In flight before bob exists.
        let pre_join = alice
            .build_message(&provider_a, &signer_a, &chat)
            .expect("chat at epoch 0");

        let provider_b = TestProvider::default();
        let (bob_kp, _bob_signer) = member_key_package(&provider_b, b"bob");
        let artifacts = alice
            .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(bob_kp)])
            .expect("add bob");
        alice.merge_own_commit(&provider_a).unwrap();
        let mut bob = MlsService::new_from_welcome(
            &provider_b,
            &artifacts.welcome.unwrap(),
            MlsGroupJoinConfig::builder(),
        )
        .expect("open welcome")
        .expect("welcome addressed to bob");

        let result = bob
            .decrypt_application_only(&provider_b, &pre_join)
            .expect("a pre-join straggler is not an error");
        assert!(result.is_none(), "and it yields nothing");
    }
}
