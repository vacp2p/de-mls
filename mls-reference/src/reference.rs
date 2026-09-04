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

use openmls::credentials::CredentialWithKey;
use openmls::group::{
    GroupId, MlsGroup, MlsGroupCreateConfigBuilder, MlsGroupJoinConfigBuilder, StagedCommit,
    StagedWelcome, WelcomeError,
};
use openmls::key_packages::{KeyPackage, KeyPackageIn};
use openmls::prelude::tls_codec::Serialize as TlsSerialize;
use openmls::prelude::{
    ContentType, DeserializeBytes, LeafNodeIndex, MlsMessageBodyIn, MlsMessageIn,
    ProcessedMessageContent, ProtocolMessage, ProtocolVersion, Sender,
};
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

    /// The adds and removes a staged commit performs, in commit order. Removes
    /// resolve to a member id here, before the merge — once the commit
    /// applies, the leaf is blank and the id is gone.
    fn actions_of(&self, staged: &StagedCommit) -> Result<Vec<Action>, Error> {
        let mut actions = Vec::new();
        for add in staged.add_proposals() {
            let key_package = add.add_proposal().key_package();
            actions.push(Action::Add {
                member: key_package.leaf_node().signature_key().as_slice().to_vec(),
                key_package: key_package
                    .tls_serialize_detached()
                    .map_err(Error::KeyPackageTls)?,
            });
        }
        for remove in staged.remove_proposals() {
            let leaf = remove.remove_proposal().removed();
            if let Some(member) = self.id_at(leaf) {
                actions.push(Action::Remove { member });
            }
        }
        Ok(actions)
    }
}

impl<Pr, S> GroupOps for Reference<Pr, S>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    S: Signer,
{
    type Error = Error;

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

    fn build_commit(&mut self, actions: &[Action]) -> Result<Built, Error> {
        let mut adds: Vec<KeyPackage> = Vec::new();
        let mut removals: Vec<LeafNodeIndex> = Vec::new();

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
            }
        }
        let proposal_count = (adds.len() + removals.len()) as u32;

        // Proposals ride inline, never through the proposal store — a
        // non-empty store blocks `create_message`, muting the sender for the
        // round. `consume_proposal_store(false)`: commit exactly this batch;
        // `force_self_update(true)`: an UpdatePath (fresh entropy) even on
        // add-only commits, which MLS would otherwise let skip it.
        let bundle = self
            .group
            .commit_builder()
            .consume_proposal_store(false)
            .force_self_update(true)
            .propose_adds(adds)
            .propose_removals(removals)
            .load_psks(self.provider.storage())?
            .build(
                self.provider.rand(),
                self.provider.crypto(),
                &self.signer,
                |_| true,
            )?
            .stage_commit(&self.provider)?;

        let welcome = match bundle.to_welcome_msg() {
            Some(w) => Some(w.to_bytes()?),
            None => None,
        };
        let (commit_msg, _welcome, _group_info) = bundle.into_contents();
        let commit = commit_msg.to_bytes()?;
        let hash = commit_hash(&commit);
        self.pending = Some(hash);

        Ok(Built {
            hash,
            proposal_count,
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
        let processed = self
            .group
            .process_message(&self.provider, protocol_message)?;
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
        // Everything else was staged against the epoch we just left.
        self.staged.clear();
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
        self.pending = None;
        self.group
            .delete(self.provider.storage())
            .map_err(Error::storage)
    }
}
