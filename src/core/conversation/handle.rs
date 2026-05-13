//! Per-conversation aggregate owned by the orchestrator: protocol state, MLS
//! service, plug-ins, state machine, durable config, and operating mode.
//! App-side wiring (phase timer, auto-vote handles) lives on
//! [`crate::app::SessionRunner`], which owns one `ConversationHandle` by value.

use prost::Message;
use tracing::info;

use crate::{
    core::{
        BufferedCommitCandidate, Conversation, ConversationConfig, ConversationState,
        ConversationStateMachine, CoreError, FreezeFinalizeResult, OperatingMode,
        PeerScoringPlugin, ProcessResult, ProposalKind, StewardListPlugin, compute_commit_hash,
        finalize_freeze_round, member_set, process_inbound,
    },
    ds::{APP_MSG_SUBTOPIC, OutboundPacket},
    mls_crypto::{
        CommitCandidate as MlsCommitCandidate, KeyPackageBytes, MlsCommitInput, MlsService,
    },
    protos::de_mls::messages::v1::{
        AppMessage, CommitCandidate, conversation_update_request::Payload,
    },
};

/// Per-conversation aggregate. Fields with simple direct semantics
/// (`conversation`, `state_machine`, `config`, `scoring`, `steward`) are
/// exposed as fields; fields with attach/detach lifecycle (`mls`) or
/// RFC-defined invariants (`operating_mode`) go through controlled accessors.
pub struct ConversationHandle<M: MlsService, Sc: PeerScoringPlugin, St: StewardListPlugin> {
    pub(crate) conversation: Conversation,
    /// Per-conversation MLS service. `None` for joiners in `PendingJoin` who
    /// haven't accepted a welcome yet; once attached via
    /// [`Self::attach_mls`] it stays `Some` for the handle's lifetime.
    mls: Option<M>,
    /// Per-conversation state machine. The orchestrator updates this together
    /// with the phase timer so the two never drift.
    pub(crate) state_machine: ConversationStateMachine,
    /// Per-conversation durable config: voting/consensus durations,
    /// `liveness_criteria_yes`, `pending_update_max_epochs`. Read by
    /// orchestrators; joiner-sync writes through this directly.
    pub(crate) config: ConversationConfig,
    /// Per-conversation peer-score plug-in. Read by protocol decisions under
    /// the orchestrator's per-handle lock; no separate `Mutex` needed.
    pub(crate) scoring: Sc,
    /// Per-conversation steward-list plug-in. Holds the active list, retry
    /// counter, and election retry policy. Orchestrator composes
    /// eligibility from MLS members + `Conversation::is_pending_removal` and
    /// passes it on every position query.
    pub(crate) steward_list: St,
    /// Authorization mode (RFC §Layer 3 Anti-Deadlock ECP). `Recovery` is
    /// set when an accepted Deadlock ECP relaxes the steward gate so any
    /// member may produce the next commit; cleared on accepted election.
    /// Read by the freeze coordinator, the create-commit path, and
    /// `core::finalize_freeze_round` (via `in_recovery` parameter).
    operating_mode: OperatingMode,
}

impl<M: MlsService, Sc: PeerScoringPlugin, St: StewardListPlugin> ConversationHandle<M, Sc, St> {
    /// Build a fresh handle. Creator path passes `Some(mls)`; joiner
    /// path passes `None` and attaches later via [`Self::attach_mls`].
    pub(crate) fn new(
        conversation: Conversation,
        mls: Option<M>,
        state_machine: ConversationStateMachine,
        config: ConversationConfig,
        scoring: Sc,
        steward_list: St,
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

    pub(crate) fn is_in_recovery_mode(&self) -> bool {
        self.operating_mode == OperatingMode::Recovery
    }

    pub(crate) fn enter_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Recovery;
    }

    pub(crate) fn exit_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Normal;
    }

    // ── State accessor ──────────────────────────────────────────────

    pub(crate) fn current_state(&self) -> ConversationState {
        self.state_machine.current_state()
    }

    // ── MLS service ─────────────────────────────────────────────────

    /// Borrow the MLS service, if attached. `None` for joiners
    /// pre-welcome.
    pub(crate) fn mls(&self) -> Option<&M> {
        self.mls.as_ref()
    }

    /// Borrow the MLS service, erroring with
    /// [`CoreError::MlsGroupNotInitialized`] when not attached. Use this
    /// in code paths where the service must be present so the `?` chain
    /// stays linear.
    pub(crate) fn expect_mls(&self) -> Result<&M, CoreError> {
        self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)
    }

    /// Attach an MLS service. Called by joiners after the welcome
    /// arrives. Caller is responsible for not double-attaching.
    pub(crate) fn attach_mls(&mut self, mls: M) {
        self.mls = Some(mls);
    }

    /// Drop the attached MLS service and return it. Used on conversation leave
    /// so the caller can run service-side cleanup (`mls.delete()`).
    pub(crate) fn take_mls(&mut self) -> Option<M> {
        self.mls.take()
    }

    // ── Protocol-function wrappers ─────────────────────────────────
    //
    // Read `conversation`, `mls`, and `steward` from `self` so coordinator
    // callsites don't destructure the entry. Protocol logic lives in
    // sibling `core` modules; these are pure delegation.

    /// Current MLS members; errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no service is attached
    /// (joiner pre-welcome).
    pub(crate) fn conversation_members(&self) -> Result<Vec<Vec<u8>>, CoreError> {
        match &self.mls {
            Some(mls) => Ok(mls.members()?),
            None => Err(CoreError::MlsGroupNotInitialized),
        }
    }

    /// Build a commit candidate. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached.
    pub(crate) fn create_commit_candidate(
        &mut self,
        self_identity: &[u8],
        app_id: &[u8],
    ) -> Result<Option<OutboundPacket>, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        if !self.steward_list.is_steward(self_identity) && !self.is_in_recovery_mode() {
            return Err(CoreError::NotASteward);
        }

        if self.conversation.approved_proposals().is_empty() {
            return Err(CoreError::NoProposals);
        }

        // MLS forbids committing one's own removal. If the approved batch contains
        // RemoveMember(self), skip local candidate creation — another steward will
        // commit the batch (including this node's removal) once they enter freeze.
        if self.conversation.is_pending_removal(self_identity) {
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

        // Drop approved entries already reflected in conversation state (stale
        // rebroadcast KPs, duplicate removes) — without this MLS would reject
        // the whole batch with "Duplicate signature key in proposals and conversation".
        let current_members = mls.members()?;
        let current_members_set = member_set(&current_members);
        let is_member = |id: &[u8]| current_members_set.contains(id);

        // Urgent (ECP-driven) freeze: restrict the batch to just the target's
        // RemoveMember. See `Conversation::urgent_commit_target`.
        let urgent_target = self.conversation.urgent_commit_target().map(|t| t.to_vec());

        // Iterate in insertion order (FIFO): library proposal IDs are
        // content-derived hashes, so sort-by-id is not temporal.
        let k_max = mls.commit_batch_max();
        let mut updates = Vec::with_capacity(self.conversation.approved_order().len().min(k_max));
        for pid in self.conversation.approved_order() {
            if updates.len() >= k_max {
                break;
            }
            let Some(proposal) = self.conversation.approved_proposals().get(pid) else {
                continue;
            };
            match proposal.payload.as_ref() {
                Some(Payload::InviteMember(im)) => {
                    if urgent_target.is_some() {
                        continue;
                    }
                    if is_member(&im.identity) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Add(KeyPackageBytes::new(
                        im.key_package_bytes.clone(),
                        im.identity.clone(),
                    )));
                }
                Some(Payload::RemoveMember(rm)) => {
                    if let Some(target) = urgent_target.as_deref()
                        && rm.identity != target
                    {
                        continue;
                    }
                    if !is_member(&rm.identity) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Remove(rm.identity.clone()));
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
            conversation_name: self.conversation.name_bytes().to_vec(),
            mls_proposals,
            commit_message: commit,
            steward_identity: self_identity.to_vec(),
        };

        // Welcome bytes are deferred: sent from finalize_freeze_round after the
        // commit merges, so joiners can't advance epoch ahead of the steward.
        let commit_hash = compute_commit_hash(&candidate.commit_message);
        let epoch = mls.current_epoch()?;
        let _ = self.conversation.add_freeze_candidate(
            BufferedCommitCandidate {
                candidate_msg: candidate.clone(),
                commit_hash,
                is_local_candidate: true,
                welcome_bytes: welcome,
            },
            epoch,
        );

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

    /// Finalize the active freeze round. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached.
    pub(crate) fn finalize_freeze_round(
        &mut self,
        allow_subset_candidates: bool,
        app_id: &[u8],
        self_identity: &[u8],
    ) -> Result<FreezeFinalizeResult, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        let in_recovery = self.operating_mode == OperatingMode::Recovery;
        finalize_freeze_round(
            &mut self.conversation,
            mls,
            &self.steward_list,
            in_recovery,
            allow_subset_candidates,
            app_id,
            self_identity,
        )
    }

    /// Process an inbound app-subtopic payload. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached — caller should check `mls().is_some()` first.
    pub(crate) fn process_inbound(&mut self, payload: &[u8]) -> Result<ProcessResult, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        process_inbound(&mut self.conversation, mls, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{StubScoring, StubStewardList, UnusedMls};

    fn make_handle(
        steward_list: StubStewardList,
    ) -> ConversationHandle<UnusedMls, StubScoring, StubStewardList> {
        ConversationHandle::new(
            Conversation::new("g"),
            Some(UnusedMls),
            ConversationStateMachine::new_as_member(),
            ConversationConfig::default(),
            StubScoring,
            steward_list,
        )
    }

    #[test]
    fn create_commit_candidate_errors_for_non_steward_outside_recovery() {
        let mut handle = make_handle(StubStewardList::member());
        let err = handle
            .create_commit_candidate(b"me", b"app")
            .expect_err("non-steward should be rejected");
        assert!(matches!(err, CoreError::NotASteward));
    }

    #[test]
    fn create_commit_candidate_errors_when_no_approved_proposals() {
        let mut handle = make_handle(StubStewardList::steward());
        let err = handle
            .create_commit_candidate(b"me", b"app")
            .expect_err("empty approved queue should be rejected");
        assert!(matches!(err, CoreError::NoProposals));
    }
}
