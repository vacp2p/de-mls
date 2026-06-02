//! Per-conversation aggregate owned by the orchestrator: protocol state, MLS
//! service, plug-ins, state machine, durable config, and operating mode.

use prost::Message;
use tracing::info;

use crate::{
    core::{
        BufferedCommitCandidate, ConversationConfig, ConversationPluginsFactory,
        ConversationQueues, ConversationState, ConversationStateMachine, CoreError,
        FreezeBufferOutcome, FreezeFinalizeResult, OperatingMode, ProcessResult, ProposalKind,
        StewardListPlugin, compute_commit_hash, finalize_freeze_round, member_set, process_inbound,
    },
    ds::{APP_MSG_SUBTOPIC, OutboundPacket},
    mls_crypto::{
        CommitCandidate as MlsCommitCandidate, KeyPackageBytes, MlsCommitInput, MlsService,
    },
    protos::de_mls::messages::v1::{
        AppMessage, CommitCandidate, conversation_update_request::Payload,
    },
};

/// Per-conversation aggregate owned by the orchestrator. Bundles protocol
/// state, the MLS service, plug-ins, state machine, durable config, and
/// operating mode behind one type parameter.
pub struct Conversation<CP: ConversationPluginsFactory> {
    pub conversation: ConversationQueues,
    /// Per-conversation MLS service. `None` for joiners in `PendingJoin` who
    /// haven't accepted a welcome yet; once attached via
    /// [`Self::attach_mls`] it stays `Some` for the conversation's lifetime.
    mls: Option<CP::Mls>,
    pub(crate) state_machine: ConversationStateMachine,
    /// Per-conversation durable config: voting/consensus durations,
    /// `liveness_criteria_yes`, `pending_update_max_epochs`.
    pub(crate) config: ConversationConfig,
    /// Per-conversation peer-score plug-in.
    pub(crate) scoring: CP::Scoring,
    /// Per-conversation steward-list plug-in.
    pub(crate) steward_list: CP::StewardList,
    /// Authorization mode (RFC §Layer 3 Anti-Deadlock ECP).
    operating_mode: OperatingMode,
}

impl<CP: ConversationPluginsFactory> Conversation<CP> {
    /// Build a fresh conversation. Creator path passes `Some(mls)`; joiner
    /// path passes `None` and attaches later via [`Self::attach_mls`].
    pub(crate) fn new(
        conversation: ConversationQueues,
        mls: Option<CP::Mls>,
        state_machine: ConversationStateMachine,
        config: ConversationConfig,
        scoring: CP::Scoring,
        steward_list: CP::StewardList,
    ) -> Self {
        Self {
            conversation,
            mls,
            state_machine,
            config,
            scoring,
            steward_list,
            operating_mode: OperatingMode::Normal,
        }
    }

    // ── Operating mode (Layer 3 Anti-Deadlock) ──────────────────────

    pub fn is_in_recovery_mode(&self) -> bool {
        self.operating_mode == OperatingMode::Recovery
    }

    pub fn enter_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Recovery;
    }

    pub fn exit_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Normal;
    }

    // ── State accessor ──────────────────────────────────────────────

    pub fn current_state(&self) -> ConversationState {
        self.state_machine.current_state()
    }

    // ── MLS service ─────────────────────────────────────────────────

    /// Borrow the MLS service, if attached. `None` for joiners
    /// pre-welcome.
    pub(crate) fn mls(&self) -> Option<&CP::Mls> {
        self.mls.as_ref()
    }

    /// Borrow the MLS service, erroring with
    /// [`CoreError::MlsGroupNotInitialized`] when not attached.
    pub(crate) fn expect_mls(&self) -> Result<&CP::Mls, CoreError> {
        self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)
    }

    /// Mutable [`Self::expect_mls`] — required for the commit pipeline
    /// and encrypt/decrypt methods that advance MLS state.
    pub(crate) fn expect_mls_mut(&mut self) -> Result<&mut CP::Mls, CoreError> {
        self.mls.as_mut().ok_or(CoreError::MlsGroupNotInitialized)
    }

    /// Attach an MLS service (joiner, post-welcome). Overwrites, so the
    /// caller must only attach when `mls().is_none()` — a double-attach
    /// drops live group state.
    pub(crate) fn attach_mls(&mut self, mls: CP::Mls) {
        self.mls = Some(mls);
    }

    /// Drop the attached MLS service and return it. Used on conversation leave
    /// so the caller can run service-side cleanup (`mls.delete()`).
    pub(crate) fn take_mls(&mut self) -> Option<CP::Mls> {
        self.mls.take()
    }

    // ── Protocol-function wrappers ─────────────────────────────────
    //
    // Read `conversation`, `mls`, and `steward` from `self` so coordinator
    // callsites don't destructure the entry. Protocol logic lives in
    // sibling `core` modules; these are pure delegation.

    /// Build a commit candidate. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached.
    pub(crate) fn create_commit_candidate(
        &mut self,
        self_member_id: &[u8],
        app_id: &[u8],
    ) -> Result<Option<OutboundPacket>, CoreError> {
        if self.mls.is_none() {
            return Err(CoreError::MlsGroupNotInitialized);
        }
        if !self.steward_list.is_steward(self_member_id) && !self.is_in_recovery_mode() {
            return Err(CoreError::NotASteward);
        }

        if self.conversation.approved_proposals().is_empty() {
            return Err(CoreError::NoProposals);
        }

        // MLS forbids committing one's own removal. If the approved batch contains
        // RemoveMember(self), skip local candidate creation — another steward will
        // commit the batch (including this node's removal) once they enter freeze.
        if self.conversation.has_approved_removal(self_member_id) {
            info!(
                conversation = self.conversation.name(),
                "commit candidate skipped: approved batch contains self-remove"
            );
            return Ok(None);
        }

        // Governance proposals (emergency, election) are consensus-only and must
        // not be in the approved queue at batch creation time.
        let non_mls_ids: Vec<u32> = self
            .conversation
            .approved_proposals()
            .iter()
            .filter(|(_, req)| ProposalKind::of(req).is_governance())
            .map(|(&id, _)| id)
            .collect();

        if !non_mls_ids.is_empty() {
            return Err(CoreError::UnexpectedNonMlsProposals {
                proposal_ids: non_mls_ids,
            });
        }

        // Borrow `self.mls` directly so later `self.conversation` reads stay
        // a disjoint borrow.
        let mls = self.mls.as_mut().ok_or(CoreError::MlsGroupNotInitialized)?;

        // Drop approved entries already reflected in conversation state (stale
        // rebroadcast KPs, duplicate removes) — without this MLS would reject
        // the whole batch with "Duplicate signature key in proposals and conversation".
        let current_members = mls.members()?;
        let current_members_set = member_set(&current_members);
        let is_member = |id: &[u8]| current_members_set.contains(id);

        // Urgent (ECP-driven) freeze: restrict the batch to just the target's
        // RemoveMember. See `ConversationQueues::urgent_commit_target`.
        let urgent_target = self.conversation.urgent_commit_target().map(|t| t.to_vec());

        // Iterate in insertion order (FIFO): library proposal IDs are
        // content-derived hashes, so sort-by-id is not temporal.
        let k_max = mls.commit_batch_max();
        let approved = self.conversation.approved_proposals();
        let mut updates = Vec::with_capacity(approved.len().min(k_max));
        for (_pid, proposal) in approved.iter().take(k_max) {
            match proposal.payload.as_ref() {
                Some(Payload::MemberInvite(im)) => {
                    if urgent_target.is_some() {
                        continue;
                    }
                    if is_member(&im.member_id) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Add(KeyPackageBytes::new(
                        im.key_package_bytes.clone(),
                        im.member_id.clone(),
                    )));
                }
                Some(Payload::RemoveMember(rm)) => {
                    if let Some(target) = urgent_target.as_deref()
                        && rm.member_id != target
                    {
                        continue;
                    }
                    if !is_member(&rm.member_id) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Remove(rm.member_id.clone()));
                }
                _ => return Err(CoreError::InvalidConversationUpdateRequest),
            }
        }

        if updates.is_empty() {
            return Ok(None);
        }

        let MlsCommitCandidate {
            proposals: mls_proposals,
            commit,
            welcome,
        } = mls.create_commit_candidate(&updates)?;

        let candidate = CommitCandidate {
            conversation_id: self.conversation.name_bytes().to_vec(),
            mls_proposals,
            commit_message: commit,
            steward_member_id: self_member_id.to_vec(),
        };

        // Welcome bytes are deferred until our merge so joiners can't
        // advance epoch ahead of the steward.
        let commit_hash = compute_commit_hash(&candidate.commit_message);
        let epoch = mls.current_epoch()?;
        let max_candidates = mls.members()?.len();
        let outcome = self.conversation.add_freeze_candidate(
            BufferedCommitCandidate {
                candidate_msg: candidate.clone(),
                commit_hash,
                is_local_candidate: true,
                welcome_bytes: welcome,
            },
            epoch,
            max_candidates,
        );
        // Non-Buffered outcomes are legitimate runtime states (see
        // `FreezeBufferOutcome`), not errors — log at debug.
        if !matches!(outcome, FreezeBufferOutcome::Buffered) {
            tracing::debug!(
                conversation = self.conversation.name(),
                epoch,
                ?outcome,
                "local commit candidate not buffered",
            );
        }

        info!(
            conversation = self.conversation.name(),
            epoch,
            proposals = updates.len(),
            "commit candidate created"
        );

        let candidate_msg: AppMessage = candidate.into();
        Ok(Some(OutboundPacket::new(
            candidate_msg.encode_to_vec(),
            APP_MSG_SUBTOPIC,
            self.conversation.name(),
            app_id,
        )))
    }

    /// Finalize the active freeze round.
    pub(crate) fn finalize_freeze_round(
        &mut self,
        allow_subset_candidates: bool,
        self_member_id: &[u8],
    ) -> Result<FreezeFinalizeResult, CoreError> {
        let in_recovery = self.operating_mode == OperatingMode::Recovery;
        let mls = self.mls.as_mut().ok_or(CoreError::MlsGroupNotInitialized)?;
        finalize_freeze_round(
            &mut self.conversation,
            mls,
            &self.steward_list,
            in_recovery,
            allow_subset_candidates,
            self_member_id,
        )
    }

    /// Process an inbound app-subtopic payload. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached — caller should check `mls().is_some()` first.
    pub(crate) fn process_inbound(&mut self, payload: &[u8]) -> Result<ProcessResult, CoreError> {
        let mls = self.mls.as_mut().ok_or(CoreError::MlsGroupNotInitialized)?;
        process_inbound(&mut self.conversation, mls, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{StubPluginsFactory, StubScoring, StubStewardList, UnusedMls};

    fn make_conversation(steward_list: StubStewardList) -> Conversation<StubPluginsFactory> {
        Conversation::new(
            ConversationQueues::new("g"),
            Some(UnusedMls),
            ConversationStateMachine::new_as_member(),
            ConversationConfig::default(),
            StubScoring,
            steward_list,
        )
    }

    #[test]
    fn create_commit_candidate_errors_for_non_steward_outside_recovery() {
        let mut conversation = make_conversation(StubStewardList::member());
        let err = conversation
            .create_commit_candidate(b"me", b"app")
            .expect_err("non-steward should be rejected");
        assert!(matches!(err, CoreError::NotASteward));
    }

    #[test]
    fn create_commit_candidate_errors_when_no_approved_proposals() {
        let mut conversation = make_conversation(StubStewardList::steward());
        let err = conversation
            .create_commit_candidate(b"me", b"app")
            .expect_err("empty approved queue should be rejected");
        assert!(matches!(err, CoreError::NoProposals));
    }

    /// An emergency-criteria proposal in the approved queue must surface as
    /// `UnexpectedNonMlsProposals` — only MLS-producing payloads belong in a
    /// commit. The error carries the offending proposal ids so the
    /// orchestrator can drop them.
    #[test]
    fn create_commit_candidate_errors_on_emergency_in_approved_queue() {
        use crate::protos::de_mls::messages::v1::ViolationEvidence;

        let mut conversation = make_conversation(StubStewardList::steward());
        let emergency = ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new())
            .with_creator(vec![0x01])
            .into_update_request()
            .unwrap();
        conversation
            .conversation
            .insert_approved_proposal(50, emergency);

        let err = conversation
            .create_commit_candidate(b"me", b"app")
            .expect_err("emergency in approved queue should be rejected");
        let CoreError::UnexpectedNonMlsProposals { proposal_ids } = err else {
            panic!("expected UnexpectedNonMlsProposals, got {err:?}");
        };
        assert_eq!(proposal_ids, vec![50]);
    }
}
