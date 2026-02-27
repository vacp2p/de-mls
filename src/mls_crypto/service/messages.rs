use super::*;

impl<S> MlsService<S>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // ══════════════════════════════════════════════════════════
    // Messages
    // ══════════════════════════════════════════════════════════

    /// Encrypt an application message for the group.
    ///
    /// Returns MLS ciphertext that only group members can decrypt.
    pub fn encrypt(&self, group_id: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
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

        let message = group.create_message(&provider, &identity.signer, plaintext)?;
        Ok(message.to_bytes()?)
    }

    /// Decrypt an inbound MLS message, accepting only application messages.
    ///
    /// Unlike [`decrypt`], this method does NOT store proposals in the MLS
    /// pending queue. If a proposal or commit is encountered on the app
    /// subtopic, it is ignored — preventing MLS state pollution from rogue
    /// messages on the wrong channel.
    ///
    /// Use this for normal app messages on APP_MSG_SUBTOPIC.
    pub fn decrypt_application_only(
        &self,
        group_id: &str,
        ciphertext: &[u8],
    ) -> Result<DecryptResult> {
        let provider = self.make_provider();

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
            return Ok(DecryptResult::Ignored);
        }

        // Ignore messages from any non-current epoch — OpenMLS rejects both
        // old and future epochs, so gracefully ignore both to avoid hard errors
        // (e.g. when a joiner sends at epoch N+1 before we've merged our commit).
        if protocol_message.epoch() != group.epoch() {
            return Ok(DecryptResult::Ignored);
        }

        // Reject commits and proposals before process_message to avoid MLS
        // errors (e.g. MissingProposal when commit's proposals aren't stored).
        match protocol_message.content_type() {
            openmls::prelude::ContentType::Commit | openmls::prelude::ContentType::Proposal => {
                return Ok(DecryptResult::Ignored);
            }
            openmls::prelude::ContentType::Application => {}
        }

        let processed = group.process_message(&provider, protocol_message)?;
        let sender_identity = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(DecryptResult::Application(
                app.into_bytes(),
                sender_identity,
            )),
            _ => Ok(DecryptResult::Ignored),
        }
    }

    /// Process an MLS proposal from a commit candidate and store it in the
    /// MLS group's pending queue.
    ///
    /// This is used exclusively in the candidate pipeline during
    /// `finalize_freeze_round` Phase 3. Each MLS proposal from the chosen
    /// candidate is processed here so that the subsequent `process_commit()`
    /// can reference them.
    ///
    /// Returns `DecryptResult::ProposalStored` with the MLS-authenticated
    /// sender identity and the proposal action.
    pub fn process_candidate_proposal(
        &self,
        group_id: &str,
        proposal_bytes: &[u8],
    ) -> Result<DecryptResult> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(proposal_bytes)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        let processed = group.process_message(&provider, protocol_message)?;
        let sender_identity = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ProposalMessage(proposal) => {
                let action = Self::extract_proposal_action(group, proposal.proposal());

                // Store in MLS pending queue — required for commit processing
                group.store_pending_proposal(provider.storage(), proposal.as_ref().clone())?;
                Ok(DecryptResult::ProposalStored(sender_identity, action))
            }
            _ => Err(MlsError::Service(MlsServiceError::UnexpectedMessageType)),
        }
    }

    /// Decrypt/process an inbound MLS message.
    ///
    /// Handles application messages and proposals.
    ///
    /// Commit messages are intentionally rejected in this path and must be
    /// processed via [`process_commit`] + explicit merge/discard.
    pub fn decrypt(&self, group_id: &str, ciphertext: &[u8]) -> Result<DecryptResult> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        // Check group ID
        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            return Ok(DecryptResult::Ignored);
        }

        // Ignore messages from any non-current epoch. Old epochs can't be
        // processed, and future epochs arrive when a joiner sends at epoch N+1
        // before we've merged our pending commit.
        if protocol_message.epoch() != group.epoch() {
            tracing::debug!(
                "Ignoring message from epoch {} (current: {})",
                protocol_message.epoch().as_u64(),
                group.epoch().as_u64()
            );
            return Ok(DecryptResult::Ignored);
        }

        if protocol_message.content_type() == openmls::prelude::ContentType::Commit {
            tracing::debug!(
                "Ignoring commit on decrypt() path for group {}: use process_commit() instead",
                group_id,
            );
            return Ok(DecryptResult::Ignored);
        }

        let processed = group.process_message(&provider, protocol_message)?;
        let sender_identity = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(DecryptResult::Application(
                app.into_bytes(),
                sender_identity,
            )),
            ProcessedMessageContent::ProposalMessage(proposal) => {
                // Extract the action before storing (consuming) the proposal.
                let action = Self::extract_proposal_action(group, proposal.proposal());

                group.store_pending_proposal(provider.storage(), proposal.as_ref().clone())?;
                Ok(DecryptResult::ProposalStored(sender_identity, action))
            }
            ProcessedMessageContent::StagedCommitMessage(_) => Ok(DecryptResult::Ignored),
            ProcessedMessageContent::ExternalJoinProposalMessage(_) => Ok(DecryptResult::Ignored),
        }
    }

    /// Inspect the untrusted outer kind of an MLS message.
    ///
    /// This is useful for strict lane checks (proposal lane vs commit lane)
    /// before deeper processing.
    pub fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(message_bytes)?;
        let protocol = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(_) => return Ok(MlsMessageKind::Welcome),
            MlsMessageBodyIn::PrivateMessage(m) => ProtocolMessage::PrivateMessage(m),
            MlsMessageBodyIn::PublicMessage(m) => ProtocolMessage::PublicMessage(Box::new(m)),
            _ => return Ok(MlsMessageKind::Other),
        };

        let kind = match protocol.content_type() {
            openmls::prelude::ContentType::Application => MlsMessageKind::Application,
            openmls::prelude::ContentType::Proposal => MlsMessageKind::Proposal,
            openmls::prelude::ContentType::Commit => MlsMessageKind::Commit,
        };
        Ok(kind)
    }
}
