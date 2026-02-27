use super::*;

impl<S> MlsService<S>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // ══════════════════════════════════════════════════════════
    // Steward
    // ══════════════════════════════════════════════════════════

    /// Create proposals for membership changes and stage a commit candidate.
    ///
    /// This is the core steward operation: takes a list of add/remove
    /// operations, creates MLS proposals, and produces a commit candidate.
    ///
    /// This does NOT merge the pending commit.
    pub fn create_commit_candidate(
        &self,
        group_id: &str,
        updates: &[GroupUpdate],
    ) -> Result<CommitCandidate> {
        let id_guard = self
            .identity
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let identity = id_guard
            .as_ref()
            .ok_or(MlsError::Identity(IdentityError::IdentityNotFound))?;

        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let mut mls_proposals = Vec::new();

        for update in updates {
            match update {
                GroupUpdate::Add(key_package) => {
                    let kp: openmls::key_packages::KeyPackage =
                        serde_json::from_slice(key_package.as_bytes())
                            .map_err(MlsServiceError::InvalidKeyPackage)?;
                    let (mls_message_out, _proposal_ref) =
                        group.propose_add_member(&provider, &identity.signer, &kp)?;
                    mls_proposals.push(mls_message_out.to_bytes()?);
                }
                GroupUpdate::Remove(wallet_bytes) => {
                    let member_index = group.members().find_map(|m| {
                        if m.credential.serialized_content() == wallet_bytes {
                            Some(m.index)
                        } else {
                            None
                        }
                    });
                    if let Some(index) = member_index {
                        let (mls_message_out, _proposal_ref) =
                            group.propose_remove_member(&provider, &identity.signer, index)?;
                        mls_proposals.push(mls_message_out.to_bytes()?);
                    }
                }
            }
        }

        let (commit_msg, welcome, _group_info) =
            group.commit_to_pending_proposals(&provider, &identity.signer)?;

        let welcome_bytes = match welcome {
            Some(w) => Some(w.to_bytes()?),
            None => None,
        };

        Ok(CommitCandidate {
            proposals: mls_proposals,
            commit: commit_msg.to_bytes()?,
            welcome: welcome_bytes,
        })
    }

    /// Merge this member's own pending commit candidate.
    pub fn merge_own_commit(&self, group_id: &str) -> Result<()> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        group.merge_pending_commit(&provider)?;
        Ok(())
    }

    /// Discard this member's own pending commit candidate.
    pub fn discard_own_commit(&self, group_id: &str) -> Result<()> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let _ = group.clear_pending_commit(provider.storage());
        let _ = group.clear_pending_proposals(provider.storage());
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Split Commit Processing (inspect → validate → merge/discard)
    // ══════════════════════════════════════════════════════════

    /// Process an inbound commit without merging it.
    ///
    /// Calls `process_message()` to authenticate the commit sender and extract
    /// the membership changes, then stores the `StagedCommit` internally.
    /// The caller should validate the batch and then call either
    /// [`merge_staged_commit`] or [`discard_staged_commit`].
    pub fn process_commit(&self, group_id: &str, ciphertext: &[u8]) -> Result<StagedCommitResult> {
        let provider = self.make_provider();

        // Phase 1: Hold only the groups lock. Do all group work (deserialize,
        // authenticate, extract actions) and produce an owned StagedCommit.
        let outcome = {
            let mut groups = self
                .groups
                .write()
                .map_err(|e| StorageError::Lock(e.to_string()))?;
            let group = groups.get_mut(group_id).ok_or_else(|| {
                MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
            })?;

            let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
            let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

            if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
                tracing::debug!(
                    "process_commit: ignoring commit for wrong group ID (expected {})",
                    group_id,
                );
                return Ok(StagedCommitResult::Ignored);
            }

            if protocol_message.epoch() < group.epoch() {
                tracing::debug!(
                    "process_commit: ignoring stale commit from epoch {} (current: {})",
                    protocol_message.epoch().as_u64(),
                    group.epoch().as_u64(),
                );
                return Ok(StagedCommitResult::Ignored);
            }

            let processed = group.process_message(&provider, protocol_message)?;
            let sender_identity = processed.credential().serialized_content().to_vec();

            match processed.into_content() {
                ProcessedMessageContent::StagedCommitMessage(staged) => {
                    let self_removed = staged.self_removed();

                    let mut actions = Vec::new();
                    for add in staged.add_proposals() {
                        let id = add
                            .add_proposal()
                            .key_package()
                            .leaf_node()
                            .credential()
                            .serialized_content()
                            .to_vec();
                        actions.push(MlsProposalAction::Add(id));
                    }
                    for remove in staged.remove_proposals() {
                        let removed_index = remove.remove_proposal().removed();
                        let id = group
                            .member(removed_index)
                            .map(|c| c.serialized_content().to_vec())
                            .unwrap_or_default();
                        actions.push(MlsProposalAction::Remove(id));
                    }

                    Some((sender_identity, self_removed, actions, *staged))
                }
                _ => {
                    tracing::debug!(
                        "process_commit: ignoring non-commit message for group {}",
                        group_id,
                    );
                    None
                }
            }
        }; // groups lock released

        // Phase 2: Store the staged commit under its own lock.
        match outcome {
            Some((sender_identity, self_removed, actions, staged)) => {
                self.pending_staged_commits
                    .write()
                    .map_err(|e| StorageError::Lock(e.to_string()))?
                    .insert(group_id.to_string(), staged);

                Ok(StagedCommitResult::Staged {
                    sender_identity,
                    self_removed,
                    actions,
                })
            }
            None => Ok(StagedCommitResult::Ignored),
        }
    }

    /// Merge a previously staged commit, advancing the MLS epoch.
    ///
    /// Call this after validation passes.
    pub fn merge_staged_commit(&self, group_id: &str) -> Result<()> {
        let provider = self.make_provider();

        // Phase 1: Take the staged commit out of the pending map.
        let staged = self
            .pending_staged_commits
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .remove(group_id)
            .ok_or_else(|| {
                MlsError::Service(MlsServiceError::NoPendingStagedCommit(group_id.to_string()))
            })?;
        // pending_staged_commits lock released — staged is an owned value.

        // Phase 2: Merge into the group under the groups lock.
        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        group.merge_staged_commit(&provider, staged)?;
        Ok(())
    }

    /// Discard a previously staged commit without merging.
    ///
    /// Call this when validation detects a violation or when a pre-authentication
    /// failure requires cleanup. Also clears pending proposals from MLS storage
    /// to keep the group state clean.
    pub fn discard_staged_commit(&self, group_id: &str) -> Result<()> {
        // Phase 1: Remove the staged commit (drop it).
        self.pending_staged_commits
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .remove(group_id);
        // pending_staged_commits lock released.

        // Phase 2: Clear pending proposals under the groups lock.
        let provider = self.make_provider();
        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        if let Some(group) = groups.get_mut(group_id) {
            let _ = group.clear_pending_proposals(provider.storage());
        }

        Ok(())
    }
}
