//! [`Reference`] — an OpenMLS 0.8.1 implementation of [`GroupOps`].
//!
//! One group, the commits staged against it, and the provider and signer it
//! runs over. It is a worked example of the contract rather than a component
//! to depend on; an application is expected to fold it into its own group
//! code.
//!
//! **Provider ownership.** [`Reference`] owns its provider and signer, because
//! [`GroupOps`] takes neither. A storage-bearing provider must not be cloned —
//! two copies of one group's storage fork the group — so an application that
//! shares one provider across conversations passes a handle to it (an `Arc`
//! around a provider whose storage is itself shared) and keeps exactly one
//! live instance per storage scope.

use std::collections::HashMap;
use std::error::Error as StdError;

use openmls::ciphersuite::hash_ref::ProposalRef;
use openmls::credentials::CredentialWithKey;
use openmls::group::{
    CommitMessageBundle, GroupId, MlsGroup, MlsGroupCreateConfigBuilder, MlsGroupJoinConfigBuilder,
    StagedCommit, StagedWelcome, WelcomeError,
};
use openmls::key_packages::{KeyPackage, KeyPackageIn};
use openmls::prelude::tls_codec::Serialize as TlsSerialize;
use openmls::prelude::{
    ContentType, DeserializeBytes, LeafNodeIndex, MlsMessageBodyIn, MlsMessageIn,
    ProcessMessageError, ProcessedMessageContent, Proposal, ProtocolMessage, ProtocolVersion,
    QueuedProposal, Sender, StageCommitError,
};
use openmls::treesync::LeafNodeParameters;
use openmls_traits::storage::StorageProvider;
use openmls_traits::{OpenMlsProvider, signatures::Signer};
use sha2::{Digest, Sha256};

use crate::contract::{Action, Applied, Built, GroupOps, Opened, StagedFacts};
use crate::error::Error;
use crate::group_config::{PAST_EPOCH_WINDOW, pin, pin_join};

/// The name of a commit: SHA-256 over its serialized bytes.
pub fn commit_hash(commit: &[u8]) -> [u8; 32] {
    Sha256::digest(commit).into()
}

/// Parse and cryptographically validate a key package, returning the validated
/// [`KeyPackage`]. This is the gate a joiner passes to enter the group; call it
/// before proposing an add so a malformed or invalid key package fails at the
/// caller rather than only when a steward commits it.
pub fn validate_key_package<Pr>(
    provider: &Pr,
    key_package_bytes: &[u8],
) -> Result<KeyPackage, Error>
where
    Pr: OpenMlsProvider,
{
    let (kp_in, _) =
        KeyPackageIn::tls_deserialize_bytes(key_package_bytes).map_err(Error::KeyPackageTls)?;
    kp_in
        .validate(provider.crypto(), ProtocolVersion::Mls10)
        .map_err(|e| Error::KeyPackageInvalid(Box::new(e)))
}

/// The member id a key package will have once seated: its leaf signature key.
/// Parsed without validating the key package — validation happens at the
/// commit. Keyed on the signature key rather than the credential because MLS
/// requires signature keys to be unique within a group, while a credential is
/// application-supplied and may repeat across members.
pub fn key_package_identity(key_package_bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let (kp_in, _) =
        KeyPackageIn::tls_deserialize_bytes(key_package_bytes).map_err(Error::KeyPackageTls)?;
    Ok(kp_in
        .unverified_credential()
        .signature_key
        .as_slice()
        .to_vec())
}

/// One conversation's MLS group, the commits staged against it, and the
/// provider and signer it runs over.
pub struct Reference<Pr, S> {
    conversation_id: String,
    group: MlsGroup,
    provider: Pr,
    signer: S,
    /// Peers' commits, held by hash between [`GroupOps::stage`] and the
    /// [`GroupOps::merge`] or [`GroupOps::discard`] that resolves them. A
    /// commit round routinely holds several at once.
    staged: HashMap<[u8; 32], StagedCommit>,
    /// The hash of our own pending commit, mirroring the group's pending-commit
    /// slot so the caller can name it.
    pending: Option<[u8; 32]>,
    /// The epoch this member entered the group at; the decrypt gate floors
    /// here, since earlier epochs used keys we never held.
    first_epoch: u64,
    /// Update proposals by proposer, ours included, held outside the group's
    /// proposal store: a non-empty store blocks `seal`. Dropped at every merge.
    held_updates: HashMap<Vec<u8>, QueuedProposal>,
    /// The parameters of our own held update: our own commit applies them
    /// through its leaf, never by reference.
    own_update: Option<LeafNodeParameters>,
}

impl<Pr, S> Reference<Pr, S>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    S: Signer,
{
    /// Create a fresh group as its sole member. The group id is the
    /// conversation id bytes, which is what lets every later message be
    /// matched to a conversation before it is decrypted.
    ///
    /// The leaf is seeded straight from `credential` — no key package, since
    /// key packages are how *joiners* are added. `group_config` carries the
    /// application's ciphersuite, capabilities and extensions; the pinned
    /// settings are stamped on top before building.
    pub fn create(
        conversation_id: String,
        provider: Pr,
        signer: S,
        credential: CredentialWithKey,
        group_config: MlsGroupCreateConfigBuilder,
    ) -> Result<Self, Error> {
        let group = MlsGroup::new_with_group_id(
            &provider,
            &signer,
            &pin(group_config),
            GroupId::from_slice(conversation_id.as_bytes()),
            credential,
        )?;

        Ok(Self {
            conversation_id,
            first_epoch: group.epoch().as_u64(),
            group,
            provider,
            signer,
            staged: HashMap::new(),
            pending: None,
            held_updates: HashMap::new(),
            own_update: None,
        })
    }

    /// Join a group from a welcome. `provider` must hold the joiner's
    /// key-package private keys from the earlier key-package build.
    ///
    /// Returns `Ok(None)` when the welcome addresses none of our key packages:
    /// on a broadcast transport every node sees every welcome, so "not for me"
    /// is the common case and must not read as a failure. The conversation id
    /// comes back out of the group id.
    pub fn open_welcome(
        provider: Pr,
        signer: S,
        welcome_bytes: &[u8],
        join_config: MlsGroupJoinConfigBuilder,
    ) -> Result<Option<Self>, Error> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Ok(None),
        };

        let config = pin_join(join_config);
        let staged = match StagedWelcome::new_from_welcome(&provider, &config, welcome, None) {
            Ok(staged) => staged,
            Err(WelcomeError::NoMatchingKeyPackage | WelcomeError::JoinerSecretNotFound) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let group = staged.into_group(&provider)?;

        let conversation_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();
        Ok(Some(Self {
            conversation_id,
            first_epoch: group.epoch().as_u64(),
            group,
            provider,
            signer,
            staged: HashMap::new(),
            pending: None,
            held_updates: HashMap::new(),
            own_update: None,
        }))
    }

    /// Reload a group from the provider's storage after a restart.
    ///
    /// `join_epoch` is the epoch this member entered the group at. MLS does
    /// not record it, and the open window is floored there, so the application
    /// persists it alongside the conversation and hands it back. Passing the
    /// group's current epoch instead is safe but blind: messages sealed at
    /// still-readable earlier epochs are then dropped.
    ///
    /// `Ok(None)` means no group is stored under that conversation id.
    pub fn load(
        conversation_id: String,
        provider: Pr,
        signer: S,
        join_epoch: u64,
    ) -> Result<Option<Self>, Error> {
        let group_id = GroupId::from_slice(conversation_id.as_bytes());
        let group = MlsGroup::load(provider.storage(), &group_id).map_err(Error::storage)?;
        Ok(group.map(|group| Self {
            conversation_id,
            group,
            provider,
            signer,
            staged: HashMap::new(),
            pending: None,
            first_epoch: join_epoch,
            held_updates: HashMap::new(),
            own_update: None,
        }))
    }

    /// Read-only access to the group, for anything the contract does not
    /// cover: the credential behind a member id, group-context extensions,
    /// exporters.
    pub fn group(&self) -> &MlsGroup {
        &self.group
    }

    /// The epoch authenticator — the value two nodes compare to prove they
    /// converged on the same epoch. Used by the conformance suite; an
    /// application needs it only for diagnostics.
    pub fn epoch_authenticator(&self) -> Vec<u8> {
        self.group.epoch_authenticator().as_slice().to_vec()
    }

    /// The leaf a member id currently sits at, or `None` when it names nobody
    /// in this group. Leaf indices exist only inside this crate; everything
    /// above it names members by signature key.
    fn leaf_of(&self, member: &[u8]) -> Option<LeafNodeIndex> {
        self.group
            .members()
            .find(|m| m.signature_key == member)
            .map(|m| m.index)
    }

    /// The member id seated at `leaf`, or `None` when the leaf is blank.
    fn id_at(&self, leaf: LeafNodeIndex) -> Option<Vec<u8>> {
        self.group.member_at(leaf).map(|m| m.signature_key)
    }

    /// The state after a merge.
    fn applied(&self) -> Applied {
        Applied {
            epoch: self.group.epoch().as_u64(),
            members: self.group.members().map(|m| m.signature_key).collect(),
        }
    }

    /// Reject a protocol message that is not for this group, and one from an
    /// epoch this group has already left behind.
    fn check_commit_window(&self, message: &ProtocolMessage) -> Result<(), Error> {
        let expected = self.group.group_id().as_slice();
        if message.group_id().as_slice() != expected {
            return Err(Error::WrongGroup {
                expected: expected.to_vec(),
                found: message.group_id().as_slice().to_vec(),
            });
        }
        if message.epoch() < self.group.epoch() {
            return Err(Error::StaleCommit {
                commit: message.epoch().as_u64(),
                current: self.group.epoch().as_u64(),
            });
        }
        Ok(())
    }

    /// The adds, removes and updates a staged commit performs, in commit
    /// order. Removes resolve to a member id here, before the merge — once
    /// the commit applies, the leaf is blank and the id is gone.
    fn actions_of(&self, staged: &StagedCommit) -> Result<Vec<Action>, Error> {
        let mut actions = Vec::new();
        for queued in staged.queued_proposals() {
            match queued.proposal() {
                Proposal::Add(add) => {
                    let key_package = add.key_package();
                    actions.push(Action::Add {
                        member: key_package.leaf_node().signature_key().as_slice().to_vec(),
                        key_package: key_package
                            .tls_serialize_detached()
                            .map_err(Error::KeyPackageTls)?,
                    });
                }
                Proposal::Remove(remove) => {
                    if let Some(member) = self.id_at(remove.removed()) {
                        actions.push(Action::Remove { member });
                    }
                }
                Proposal::Update(_) => {
                    if let Sender::Member(leaf) = queued.sender()
                        && let Some(member) = self.id_at(*leaf)
                    {
                        actions.push(Action::Update { member });
                    }
                }
                _ => {}
            }
        }
        Ok(actions)
    }

    /// Put the held updates of `members`, or all of them, into the group's
    /// proposal store for a build or a stage.
    fn load_updates(&mut self, members: Option<&[Vec<u8>]>) -> Result<Vec<ProposalRef>, Error> {
        let mut loaded = Vec::new();
        for (member, queued) in &self.held_updates {
            if members.is_some_and(|m| !m.contains(member)) {
                continue;
            }
            self.group
                .store_pending_proposal(self.provider.storage(), queued.clone())
                .map_err(Error::storage)?;
            loaded.push(queued.proposal_reference_ref().clone());
        }
        Ok(loaded)
    }

    /// Take the proposals [`Self::load_updates`] stored out again.
    fn unload_updates(&mut self, loaded: Vec<ProposalRef>) -> Result<(), Error> {
        for reference in loaded {
            self.group
                .remove_pending_proposal(self.provider.storage(), &reference)
                .map_err(Error::storage)?;
        }
        Ok(())
    }

    /// Build and stage a commit over the inline adds and removes and whatever
    /// the proposal store holds. `force_self_update(true)`: an UpdatePath
    /// (fresh entropy) even on add-only commits, which MLS would otherwise
    /// let skip it.
    fn commit_bundle(
        &mut self,
        adds: Vec<KeyPackage>,
        removals: Vec<LeafNodeIndex>,
    ) -> Result<CommitMessageBundle, Error> {
        Ok(self
            .group
            .commit_builder()
            .consume_proposal_store(true)
            .force_self_update(true)
            .leaf_node_parameters(self.own_update.clone().unwrap_or_default())
            .propose_adds(adds)
            .propose_removals(removals)
            .load_psks(self.provider.storage())?
            .build(
                self.provider.rand(),
                self.provider.crypto(),
                &self.signer,
                |_| true,
            )?
            .stage_commit(&self.provider)?)
    }
}

impl<Pr, S> GroupOps for Reference<Pr, S>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    S: Signer,
{
    type Error = Error;
    type LeafParams = LeafNodeParameters;

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn epoch(&self) -> u64 {
        self.group.epoch().as_u64()
    }

    fn own_id(&self) -> Vec<u8> {
        self.group
            .own_leaf_node()
            .map(|leaf| leaf.signature_key().as_slice().to_vec())
            .unwrap_or_default()
    }

    fn members(&self) -> Vec<Vec<u8>> {
        self.group.members().map(|m| m.signature_key).collect()
    }

    fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        // `create_message` writes the advanced ratchet through the provider
        // before returning, so the key this message used is never re-used.
        let message = self
            .group
            .create_message(&self.provider, &self.signer, plaintext)?;
        Ok(message.to_bytes()?)
    }

    fn open(&mut self, ciphertext: &[u8]) -> Result<Option<Opened>, Error> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        if protocol_message.group_id().as_slice() != self.group.group_id().as_slice() {
            return Ok(None);
        }

        // Drop epochs we hold no key for before OpenMLS raises a hard error:
        // newer than ours (a peer sealed at N+1 before we merged), older than
        // the retained window, or older than our join. None of these is the
        // sender's fault, and none is a failure of ours.
        let group_epoch = self.group.epoch().as_u64();
        let message_epoch = protocol_message.epoch().as_u64();
        if message_epoch > group_epoch
            || message_epoch < self.first_epoch
            || group_epoch - message_epoch > PAST_EPOCH_WINDOW as u64
        {
            return Ok(None);
        }

        // Commits enter through `stage` and nothing above this crate generates
        // proposals, so both are dropped here rather than fed to
        // `process_message` — which would either mutate MLS state from the
        // wrong door or fail with a confusing missing-proposal error.
        match protocol_message.content_type() {
            ContentType::Commit | ContentType::Proposal => return Ok(None),
            ContentType::Application => {}
        }

        let processed = self
            .group
            .process_message(&self.provider, protocol_message)?;

        // The sender comes from the MLS framing, which is authenticated; the
        // payload is not consulted.
        let sender = match processed.sender() {
            Sender::Member(leaf) => self
                .id_at(*leaf)
                .ok_or(Error::UnknownLeafIndex(leaf.u32()))?,
            _ => return Ok(None),
        };

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(Some(Opened {
                sender,
                epoch: message_epoch,
                plaintext: app.into_bytes(),
            })),
            _ => Ok(None),
        }
    }

    fn key_package_identity(bytes: &[u8]) -> Result<Vec<u8>, Error> {
        key_package_identity(bytes)
    }

    fn validate_key_package(&self, bytes: &[u8]) -> Result<(), Error> {
        validate_key_package(&self.provider, bytes).map(|_| ())
    }

    fn propose_update(&mut self, params: LeafNodeParameters) -> Result<Vec<u8>, Error> {
        let (message, reference) =
            self.group
                .propose_self_update(&self.provider, &self.signer, params.clone())?;
        // `propose_self_update` queues the proposal in our own store.
        let queued = self
            .group
            .pending_proposals()
            .find(|queued| queued.proposal_reference_ref() == &reference)
            .cloned()
            .ok_or(Error::MissingProposal)?;
        self.group
            .remove_pending_proposal(self.provider.storage(), &reference)
            .map_err(Error::storage)?;
        let bytes = message.to_bytes()?;
        self.held_updates.insert(self.own_id(), queued);
        self.own_update = Some(params);
        Ok(bytes)
    }

    fn validate_update(&mut self, bytes: &[u8]) -> Result<Vec<u8>, Error> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(bytes)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        let expected = self.group.group_id().as_slice();
        if protocol_message.group_id().as_slice() != expected {
            return Err(Error::WrongGroup {
                expected: expected.to_vec(),
                found: protocol_message.group_id().as_slice().to_vec(),
            });
        }
        if protocol_message.epoch() != self.group.epoch() {
            return Err(Error::StaleUpdate {
                update: protocol_message.epoch().as_u64(),
                current: self.group.epoch().as_u64(),
            });
        }
        if protocol_message.content_type() != ContentType::Proposal {
            return Err(Error::NotAnUpdate);
        }

        let processed = self
            .group
            .process_message(&self.provider, protocol_message)?;
        let member = match processed.sender() {
            Sender::Member(leaf) => self
                .id_at(*leaf)
                .ok_or(Error::UnknownLeafIndex(leaf.u32()))?,
            _ => return Err(Error::UnauthenticatedSender),
        };
        match processed.into_content() {
            ProcessedMessageContent::ProposalMessage(queued)
                if matches!(queued.proposal(), Proposal::Update(_)) =>
            {
                self.held_updates.insert(member.clone(), *queued);
                Ok(member)
            }
            _ => Err(Error::NotAnUpdate),
        }
    }

    fn build_commit(&mut self, actions: &[Action]) -> Result<Built, Error> {
        let own = self.own_id();
        let mut adds: Vec<KeyPackage> = Vec::new();
        let mut removals: Vec<LeafNodeIndex> = Vec::new();
        let mut updated: Vec<Vec<u8>> = Vec::new();

        for action in actions {
            match action {
                Action::Add {
                    member,
                    key_package,
                } => {
                    let validated = validate_key_package(&self.provider, key_package)?;
                    let found = validated.leaf_node().signature_key().as_slice();
                    if found != member {
                        return Err(Error::KeyPackageIdentityMismatch {
                            expected: member.clone(),
                            found: found.to_vec(),
                        });
                    }
                    adds.push(validated);
                }
                // A remove of somebody who already left is skipped, not an
                // error: two stewards can propose the same removal, and the
                // second commit must still be buildable.
                Action::Remove { member } => {
                    if let Some(leaf) = self.leaf_of(member) {
                        removals.push(leaf);
                    }
                }
                // Our own update rides in the commit's leaf parameters.
                Action::Update { member } => {
                    if *member == own {
                        continue;
                    }
                    if !self.held_updates.contains_key(member) {
                        return Err(Error::MissingProposal);
                    }
                    updated.push(member.clone());
                }
            }
        }

        // The updates this commit refers to are in the store only for the
        // build.
        let loaded = self.load_updates(Some(&updated))?;
        let built = self.commit_bundle(adds, removals);
        self.unload_updates(loaded)?;
        let bundle = built?;

        let welcome = match bundle.to_welcome_msg() {
            Some(w) => Some(w.to_bytes()?),
            None => None,
        };
        let (commit_msg, _welcome, _group_info) = bundle.into_contents();
        let commit = commit_msg.to_bytes()?;
        let hash = commit_hash(&commit);
        self.pending = Some(hash);

        let pending = self.group.pending_commit().ok_or(Error::UnknownCommit)?;
        let kept = self.actions_of(pending)?;

        Ok(Built {
            hash,
            proposal_count: kept.len() as u32,
            actions: kept,
            commit,
            welcome,
        })
    }

    fn pending_hash(&self) -> Option<[u8; 32]> {
        self.pending
    }

    fn stage(&mut self, commit: &[u8]) -> Result<StagedFacts, Error> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(commit)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;
        self.check_commit_window(&protocol_message)?;
        let epoch = protocol_message.epoch().as_u64();

        // Staging processes the commit without applying it, and leaves our own
        // pending commit alone — OpenMLS clears that only on a merge.
        // The held updates are in the store only for the processing.
        let loaded = self.load_updates(None)?;
        let processed = self.group.process_message(&self.provider, protocol_message);
        self.unload_updates(loaded)?;
        let processed = processed.map_err(|e| match e {
            ProcessMessageError::InvalidCommit(StageCommitError::MissingProposal) => {
                Error::MissingProposal
            }
            e => e.into(),
        })?;
        let sender = match processed.sender() {
            Sender::Member(leaf) => self.id_at(*leaf).ok_or(Error::UnauthenticatedSender)?,
            _ => return Err(Error::UnauthenticatedSender),
        };

        match processed.into_content() {
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                let self_removed = staged.self_removed();
                let actions = self.actions_of(&staged)?;
                let proposal_count = staged.queued_proposals().count() as u32;
                self.staged.insert(commit_hash(commit), *staged);
                Ok(StagedFacts {
                    sender,
                    epoch,
                    actions,
                    proposal_count,
                    self_removed,
                })
            }
            _ => Err(Error::NotACommit),
        }
    }

    fn merge(&mut self, hash: [u8; 32]) -> Result<Applied, Error> {
        if self.pending == Some(hash) {
            self.group.merge_pending_commit(&self.provider)?;
            self.pending = None;
        } else {
            let staged = self.staged.remove(&hash).ok_or(Error::UnknownCommit)?;
            // Our own candidate lost: drop it before applying the winner, or
            // the group carries a pending commit for an epoch that no longer
            // exists.
            self.clear_pending()?;
            self.group.merge_staged_commit(&self.provider, staged)?;
        }
        // Everything else was staged against the epoch we just left, and
        // updates are bound to their epoch.
        self.staged.clear();
        self.held_updates.clear();
        self.own_update = None;
        Ok(self.applied())
    }

    fn discard(&mut self, hash: [u8; 32]) {
        self.staged.remove(&hash);
    }

    fn clear_pending(&mut self) -> Result<(), Error> {
        self.group
            .clear_pending_commit(self.provider.storage())
            .map_err(Error::storage)?;
        self.pending = None;
        Ok(())
    }

    fn commit_hash(commit: &[u8]) -> [u8; 32] {
        commit_hash(commit)
    }

    fn delete(&mut self) -> Result<(), Error> {
        self.staged.clear();
        self.held_updates.clear();
        self.own_update = None;
        self.pending = None;
        self.group
            .delete(self.provider.storage())
            .map_err(Error::storage)
    }
}
