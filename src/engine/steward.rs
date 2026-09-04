//! Steward-list housekeeping on the engine: list reconciliation and
//! elections, the pending-update buffer and its drains, scoring
//! bookkeeping, and the `ConversationSync` a joiner or a degraded member
//! adopts.
//!
//! Everything here reads the facts the router last reported — [`Engine::epoch`]
//! and the member set — and writes to the wire through
//! [`Engine::send_control`].

use tracing::info;

use crate::{
    ConversationError, ConversationState, CreatorVote, DEFAULT_PEER_SCORE, ElectionDecision,
    ElectionSkip, ScoreSnapshot, ScoringConfig, StewardList, StewardListConfig,
    engine::{
        handle::Engine,
        store::EngineStore,
        types::Event,
        util::{member_set, target_member_id_of},
    },
    protos::de_mls::messages::v1::{
        ControlMessage, ConversationSync, ConversationSyncRequest, ConversationUpdateRequest,
        PeerScore, StewardElectionProposal, TimingConfig, ViolationEvidence, control_message,
        conversation_update_request,
    },
    scoring_member_diff,
};

/// Outcome of reconciling the steward list to the current epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StewardListReconcile {
    /// The list covers the epoch, or the settled members were installed
    /// locally and deterministically.
    Settled,
    /// The settled members exceed `sn_max`; the caller runs the election.
    NeedsElection,
}

impl<St: EngineStore> Engine<St> {
    // ── membership bookkeeping ─────────────────────────────────────────

    /// Add every reported member not yet tracked in scoring, and drop scored
    /// entries for members that departed. Diffing is delegated to
    /// [`scoring_member_diff`]; this method only applies the diff.
    pub(crate) fn sync_scoring_members(&mut self) -> Result<(), ConversationError> {
        let scored: Vec<Vec<u8>> = self
            .scoring
            .all_members_with_scores()?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let diff = scoring_member_diff(&scored, &self.members);
        for member_id in &diff.to_add {
            self.scoring.add_member(member_id)?;
        }
        for member_id in &diff.to_remove {
            self.scoring.remove_member(member_id)?;
        }
        // store: scores (WP3g)
        Ok(())
    }

    // ── steward list ───────────────────────────────────────────────────

    /// Reconcile the list after an epoch advance, opening a voted election
    /// when one is needed. Election-init failures are logged, not surfaced —
    /// the conversation may legitimately reject a new proposal right now.
    pub(crate) fn steward_list_housekeeping(&mut self) -> Result<(), ConversationError> {
        let reconcile = self.reconcile_steward_list()?;
        if reconcile == StewardListReconcile::NeedsElection
            && let Err(e) = self.initiate_steward_election()
        {
            info!(conversation = %self.conversation_id, error = %e, "election initiation deferred");
        }
        Ok(())
    }

    /// Reconcile the steward list to the current epoch. No-op while the list
    /// still covers it. Otherwise the settled-member count decides: `<= sn_max`
    /// installs them locally (deterministic, no vote); `> sn_max` returns
    /// [`StewardListReconcile::NeedsElection`]. Every node computes the same
    /// local list, so they agree without a round.
    pub(crate) fn reconcile_steward_list(
        &mut self,
    ) -> Result<StewardListReconcile, ConversationError> {
        let current_epoch = self.epoch;
        if !self.steward_list.is_exhausted(current_epoch) {
            return Ok(StewardListReconcile::Settled);
        }
        // Stewards are settled members only — a just-joined member can't commit
        // or vote yet, so it's excluded until the next epoch.
        let settled = self.queues.settled_members(&self.members, current_epoch);
        if self.steward_list.election_required(settled.len()) {
            return Ok(StewardListReconcile::NeedsElection);
        }
        let sn = self.steward_list.config().compute_list_size(settled.len());
        self.steward_list
            .install_list(current_epoch, &settled, sn, 0)?;
        // store: steward_list (WP3g)
        Ok(StewardListReconcile::Settled)
    }

    /// Checks that a proposed steward-election list matches what we would
    /// build from our own settled members at `election_epoch`, rejecting any
    /// list that is biased or tampered with. The local view is safe to use:
    /// membership doesn't change during an election.
    pub(crate) fn validate_election_list(
        &self,
        election: &StewardElectionProposal,
    ) -> Result<bool, ConversationError> {
        let epoch = election.election_epoch;
        let pool: Vec<Vec<u8>> = self
            .members
            .iter()
            .filter(|m| self.queues.is_settled(m, epoch))
            .cloned()
            .collect();
        self.steward_list.validate_proposed(
            &election.proposed_stewards,
            epoch,
            &pool,
            election.retry_round,
        )
    }

    /// Submit a steward-election proposal. Only the deterministic responsible
    /// proposer actually submits; others no-op, so this is safe to call on
    /// every tick without double-proposing. Forces a proposal even when the
    /// list still nominally covers the epoch: the natural-exhaustion caller
    /// already checked that, and an application-requested election
    /// ([`Engine::propose_election`]) needs to run when every remaining
    /// steward has been skipped rather than the list being exhausted.
    pub(crate) fn initiate_steward_election(&mut self) -> Result<(), ConversationError> {
        // `has_election_in_flight` is a proposal-queue check, not a
        // steward-list one — gated here, before the list call.
        if self.queues.has_election_in_flight() {
            return Ok(());
        }
        let epoch = self.epoch;
        let candidate_pool: Vec<Vec<u8>> = self
            .members
            .iter()
            .filter(|m| self.queues.is_settled(m, epoch))
            .cloned()
            .collect();

        let (proposed_stewards, election_epoch, retry_round) = {
            let queues = &self.queues;
            let eligible = |c: &[u8]| !queues.has_approved_removal(c);
            let decision = self.steward_list.propose_election(
                epoch,
                &candidate_pool,
                &self.own,
                eligible,
                true,
            )?;
            match decision {
                ElectionDecision::Skip(skip) => {
                    match skip {
                        // Bypassed by the forced call above.
                        ElectionSkip::NotExhausted => {}
                        ElectionSkip::NotResponsibleProposer => {
                            let round = self.steward_list.next_election_round();
                            let proposer = self.steward_list.responsible_proposer(
                                round,
                                &candidate_pool,
                                eligible,
                            );
                            info!(
                                conversation = %self.conversation_id,
                                proposer = ?proposer,
                                retry_round = round,
                                "skipping election: {skip}"
                            );
                        }
                        ElectionSkip::NoEligibleCandidates => {
                            info!(conversation = %self.conversation_id, "skipping election: {skip}");
                        }
                    }
                    return Ok(());
                }
                ElectionDecision::Proposed {
                    proposed_stewards,
                    election_epoch,
                    retry_round,
                } => (proposed_stewards, election_epoch, retry_round),
            }
        };

        let stewards = proposed_stewards.len();
        let request = ConversationUpdateRequest::steward_election(StewardElectionProposal {
            proposed_stewards,
            election_epoch,
            retry_round,
        });
        info!(
            conversation = %self.conversation_id,
            epoch = election_epoch,
            retry_round,
            stewards,
            "initiating steward election"
        );
        self.initiate_proposal(request, CreatorVote::Yes)
    }

    // ── unstick and score-removal proposals ─────────────────────────────

    /// File the `Deadlock` emergency proposal: on ⌈2n/3⌉ YES the current
    /// epoch steward is skipped for this epoch and the next eligible steward
    /// on the list becomes ES. Any member may file it, any time approved
    /// work is waiting for a commit — it only *proposes*, so one member can't
    /// force the outcome.
    pub(crate) fn file_deadlock_ecp(&mut self) -> Result<(), ConversationError> {
        if self.queues.approved_proposals_count() == 0 {
            return Err(ConversationError::NoProposals);
        }
        let request = ViolationEvidence::deadlock(self.epoch)
            .with_creator(self.own.clone())
            .into_update_request()?;
        info!(conversation = %self.conversation_id, epoch = self.epoch, "initiating Deadlock ECP");
        // Bundled YES: filing it is this member's own vote that the deadlock
        // is real.
        self.initiate_proposal(request, CreatorVote::Yes)
    }

    /// File the emergency proposal that removes `target` on the grounds of
    /// its peer score. Fails when `target` has no score here.
    pub(crate) fn file_score_removal_ecp(
        &mut self,
        target: &[u8],
    ) -> Result<(), ConversationError> {
        let Some(score) = self.scoring.score_for(target)? else {
            return Err(ConversationError::InvalidConversationUpdateRequest);
        };
        let request = ViolationEvidence::score_below_threshold(target.to_vec(), self.epoch, score)
            .with_creator(self.own.clone())
            .into_update_request()?;
        info!(
            conversation = %self.conversation_id,
            target = ?target,
            score,
            epoch = self.epoch,
            "initiating SCORE_BELOW_THRESHOLD ECP"
        );
        // Bundled YES: proposing it is this member's own vote for the removal.
        self.initiate_proposal(request, CreatorVote::Yes)
    }

    // ── pending-update buffer ──────────────────────────────────────────

    /// Handle a membership update (an invite or a removal request) seen from
    /// `sender`: buffer it so every member has a durable record, then promote
    /// it to a voting proposal if this node is the current epoch steward and
    /// the conversation accepts new proposals.
    pub(crate) fn handle_incoming_update_request(
        &mut self,
        sender: &[u8],
        request: ConversationUpdateRequest,
    ) -> Result<(), ConversationError> {
        // Defensive — only membership changes reach here.
        if target_member_id_of(&request).is_none() {
            return Ok(());
        }
        let state = self.current_state();
        let inserted = self
            .queues
            .insert_pending_update(request.clone(), self.epoch);
        // store: proposals (WP3g)

        // Only the epoch steward proposes immediately. The buffer survives
        // commit rounds so a later steward can retry.
        let is_epoch_steward = self.is_epoch_steward();
        let should_propose = is_epoch_steward && state == ConversationState::Working;
        info!(
            conversation = %self.conversation_id,
            epoch = self.epoch,
            sender = ?sender,
            inserted,
            buffer_total = self.queues.pending_update_count(),
            is_epoch_steward,
            state = %state,
            propose = should_propose,
            "update request buffered"
        );

        if should_propose {
            // Steward auto-propose: the steward forwards peer intent and still
            // holds a judgement call, so the proposal goes out unbundled and
            // the vote request drives the steward's vote like any other
            // member's. The proposal path may still refuse (active emergency
            // etc.) — the entry stays in the buffer for the next rotation.
            if let Err(e) = self.initiate_proposal(request, CreatorVote::Deferred) {
                self.report_deferred_proposal("steward_auto_propose", e);
            }
        }
        Ok(())
    }

    /// On epoch advance the new live epoch steward drains the pending-update
    /// buffer into voting proposals. A backup steward reaches the same drain
    /// from `tick` once `backup_takeover_window` passes (see
    /// [`Self::drive_buffered_proposals`]).
    pub(crate) fn process_buffered_updates(&mut self) -> Result<(), ConversationError> {
        if !self.is_epoch_steward() {
            return Ok(());
        }
        self.drain_buffered_updates()
    }

    /// Buffered membership updates still needing a proposal: not already
    /// covered by a live (voting or approved) proposal, and still valid — an
    /// invite for a non-member or a removal for a current member.
    pub(crate) fn actionable_buffered_updates(&self) -> Vec<ConversationUpdateRequest> {
        let members = member_set(&self.members);
        let active = self.queues.active_proposal_targets();
        self.queues
            .pending_updates()
            .iter()
            .filter(|(id, _)| !active.contains(id.as_slice()))
            .filter(|(id, p)| {
                // Drop an invite for an existing member and a removal for a
                // non-member.
                let is_member = members.contains(id.as_slice());
                match p.request.payload.as_ref() {
                    Some(conversation_update_request::Payload::MemberInvite(_)) => !is_member,
                    Some(conversation_update_request::Payload::RemoveMember(_)) => is_member,
                    _ => false,
                }
            })
            .map(|(_, p)| p.request.clone())
            .collect()
    }

    /// Promote every actionable buffered update to a voting proposal
    /// (`CreatorVote::Deferred` — the proposer relays peer intent, it doesn't
    /// endorse). Callers gate *who* drains: the epoch steward immediately via
    /// [`Self::process_buffered_updates`], a backup after
    /// `backup_takeover_window` via [`Self::drive_buffered_proposals`]. This
    /// just performs the drain.
    pub(crate) fn drain_buffered_updates(&mut self) -> Result<(), ConversationError> {
        let to_propose = self.actionable_buffered_updates();
        if to_propose.is_empty() {
            return Ok(());
        }
        info!(
            conversation = %self.conversation_id,
            count = to_propose.len(),
            "promoting buffered updates to proposals"
        );
        for request in to_propose {
            if let Err(e) = self.initiate_proposal(request, CreatorVote::Deferred) {
                self.report_deferred_proposal("drain_buffered_updates", e);
            }
        }
        Ok(())
    }

    // ── conversation sync ──────────────────────────────────────────────

    /// The `ConversationSync` control bytes that carry the current steward
    /// list, protocol flags, timing and peer scores. `None` when there is no
    /// list yet; the caller owns delivery.
    pub(crate) fn build_bootstrap(&mut self) -> Result<Option<Vec<u8>>, ConversationError> {
        // Sparse snapshot — only members whose score has diverged from
        // `default_score`, which rides along in `default_peer_score` so the
        // joiner adopts the same pivot and reads the omissions as we meant
        // them. Saves wire size at scale.
        let peer_scores: Vec<PeerScore> = self
            .scoring
            .snapshot()?
            .diverged
            .into_iter()
            .map(|(member_id, score)| PeerScore { member_id, score })
            .collect();

        let Some(list) = self.steward_list.current_list() else {
            return Ok(None);
        };
        let steward_members = list.members().to_vec();
        let election_epoch = list.election_epoch();
        let sn_min = list.config().sn_min as u32;
        let sn_max = list.config().sn_max as u32;
        // `retry_round` is the fixed seed that made this list, frozen on the
        // list rather than the next dynamic attempt. Joiners use it to
        // recompute the list order.
        let retry_round = list.retry_round();

        // Members that weren't settled at election time. A joiner uses them to
        // reconstruct the candidate pool and re-derive the list itself.
        let unsettled_members: Vec<Vec<u8>> = self
            .members
            .iter()
            .filter(|m| !self.queues.is_settled(m, election_epoch))
            .cloned()
            .collect();

        let sync = ConversationSync {
            steward_members,
            election_epoch,
            sn_min,
            sn_max,
            allow_subset_candidates: self.steward_list.config().allow_subset_candidates,
            peer_scores,
            default_peer_score: self.scoring.default_score(),
            timing: Some(TimingConfig::from(&self.config)),
            retry_round,
            max_reelection_attempts: self.steward_list.max_retries(),
            liveness_criteria_yes: self.config.liveness_criteria_yes,
            threshold_peer_score: self.scoring.threshold(),
            pending_update_max_epochs: self.config.pending_update_max_epochs,
            unsettled_members,
        };
        Ok(Some(control_bytes(
            control_message::Payload::ConversationSync(sync),
        )))
    }

    /// Broadcast the current `ConversationSync` so a bootstrap-less joiner can
    /// adopt it. No-op while there is no list to share.
    pub(crate) fn share_conversation_sync(&mut self) -> Result<(), ConversationError> {
        if let Some(bytes) = self.build_bootstrap()? {
            self.send_control(bytes);
        }
        Ok(())
    }

    /// Broadcast a `ConversationSyncRequest` so a steward re-sends its
    /// `ConversationSync`. The message carries no fields: the router hands the
    /// authenticated sender over, and that is the requester.
    pub(crate) fn broadcast_sync_request(&mut self) {
        let bytes = control_bytes(control_message::Payload::ConversationSyncRequest(
            ConversationSyncRequest {},
        ));
        self.send_control(bytes);
    }

    // ── time-driven drives ─────────────────────────────────────────────

    /// Backup-steward takeover for the pending-update buffer: the epoch
    /// steward drains it as soon as it is responsible; a backup waits out
    /// `backup_takeover_window` first, so a silent epoch steward can't strand
    /// a buffered membership change.
    pub(crate) fn drive_buffered_proposals(&mut self) -> Result<(), ConversationError> {
        let idle = self.current_state() != ConversationState::Working
            || self.queues.approved_proposals_count() > 0
            || self.actionable_buffered_updates().is_empty();
        if idle {
            self.timing.buffered_propose_anchor = None;
            return Ok(());
        }

        // The epoch steward is responsible now — propose without waiting.
        if self.is_epoch_steward() {
            self.timing.buffered_propose_anchor = None;
            return self.drain_buffered_updates();
        }
        // Only a steward can take over; a plain member keeps its backup copy.
        if !self.is_steward() {
            return Ok(());
        }

        // Backup steward: give the epoch steward `backup_takeover_window`.
        let anchor = match self.timing.buffered_propose_anchor {
            Some(anchor) => anchor,
            None => {
                self.timing.buffered_propose_anchor = Some(self.now);
                return Ok(());
            }
        };
        if self.now < anchor + self.config.backup_takeover_window {
            return Ok(());
        }
        self.timing.buffered_propose_anchor = None;
        info!(
            conversation = %self.conversation_id,
            "backup steward proposing buffered updates: epoch steward silent past backup_takeover_window"
        );
        self.drain_buffered_updates()
    }

    /// Backup-steward sync re-send takeover. The epoch steward answers a
    /// `ConversationSyncRequest` reactively; a backup arms `sync_resend_anchor`
    /// and re-sends here only if no `ConversationSync` was observed within
    /// `backup_takeover_window` — so an offline epoch steward can't strand a
    /// bootstrap-less joiner. Only a backup steward drives it; the anchor
    /// clears otherwise.
    pub(crate) fn drive_sync_resend(&mut self) -> Result<(), ConversationError> {
        let Some(anchor) = self.timing.sync_resend_anchor else {
            return Ok(());
        };
        // Only a backup takes over: the epoch steward already answered, and a
        // plain member can't. Either way the anchor no longer applies.
        if self.is_epoch_steward() || !self.is_steward() {
            self.timing.sync_resend_anchor = None;
            return Ok(());
        }
        if self.now < anchor + self.config.backup_takeover_window {
            return Ok(());
        }
        self.timing.sync_resend_anchor = None;
        info!(
            conversation = %self.conversation_id,
            "backup steward re-sending conversation sync: epoch steward silent past backup_takeover_window"
        );
        self.share_conversation_sync()
    }

    /// Pull side of sync recovery: when the local steward list is missing or
    /// exhausted at the current epoch and no election is being voted on
    /// locally, ask for the current list. Covers anyone that missed an
    /// election — a joiner welcomed that epoch, or a steward returning from
    /// offline whose stale list still names it. Rate-limited to one request
    /// per `backup_takeover_window` (the answer latency). Skipped while an
    /// election is in flight, which resolves into the list directly.
    pub(crate) fn drive_sync_request(&mut self) -> Result<(), ConversationError> {
        let needs_sync = match self.steward_list.current_list() {
            None => true,
            Some(_) => self.steward_list.is_exhausted(self.epoch),
        };
        if !needs_sync || self.queues.has_election_in_flight() {
            self.timing.sync_request_anchor = None;
            self.timing.unanswered_sync_rounds = 0;
            return Ok(());
        }
        if let Some(anchor) = self.timing.sync_request_anchor
            && self.now < anchor + self.config.backup_takeover_window
        {
            return Ok(());
        }
        self.timing.sync_request_anchor = Some(self.now);
        self.timing.unanswered_sync_rounds += 1;
        let alert_at = self.config.unanswered_sync_rounds;
        if alert_at != 0 && self.timing.unanswered_sync_rounds == alert_at {
            self.emit(Event::SyncUnanswered);
        }
        info!(
            conversation = %self.conversation_id,
            rounds = self.timing.unanswered_sync_rounds,
            "requesting conversation sync: local steward list missing or exhausted"
        );
        self.broadcast_sync_request();
        Ok(())
    }

    // ── conversation-sync adoption ─────────────────────────────────────

    /// Adopt a steward's `ConversationSync` when we hold no steward list, or
    /// one that is exhausted at the current epoch. Reconstructs the candidate
    /// pool (members minus the carried unsettled set), re-derives the steward
    /// list to verify it, then applies list, protocol flags, timing and peer
    /// scores.
    pub(crate) fn on_conversation_sync(
        &mut self,
        sync: ConversationSync,
    ) -> Result<(), ConversationError> {
        // A sync is on the wire — the request round is answered, so disarm any
        // pending backup takeover before the guard returns.
        self.timing.sync_resend_anchor = None;
        let current_epoch = self.epoch;
        // Install a first list, or replace an exhausted one with a strictly
        // newer election (higher `election_epoch`).
        if let Some(mine) = self.steward_list.election_epoch()
            && (!self.steward_list.is_exhausted(current_epoch) || sync.election_epoch <= mine)
        {
            return Ok(());
        }
        if !validate_conversation_sync(&self.conversation_id, &sync, current_epoch, &self.members)?
        {
            return Ok(());
        }

        let stewards = sync.steward_members.len();
        self.adopt_conversation_sync(&sync)?;
        self.timing.unanswered_sync_rounds = 0;
        self.emit(Event::SyncApplied);
        info!(
            conversation = %self.conversation_id,
            election_epoch = sync.election_epoch,
            stewards,
            scores = sync.peer_scores.len(),
            timing = sync.timing.is_some(),
            "conversation sync applied"
        );
        Ok(())
    }

    /// Answer a sync re-send request. The epoch steward responds now; a backup
    /// arms `sync_resend_anchor` and takes over from `tick` only if the epoch
    /// steward stays silent. The list-present guard in
    /// [`Self::on_conversation_sync`] means only the degraded requester adopts
    /// the broadcast.
    pub(crate) fn on_conversation_sync_request(
        &mut self,
        requester: &[u8],
    ) -> Result<(), ConversationError> {
        if self.is_epoch_steward() {
            self.timing.sync_resend_anchor = None;
            tracing::debug!(
                conversation = %self.conversation_id,
                requester = ?requester,
                "epoch steward re-sending conversation sync"
            );
            return self.share_conversation_sync();
        }
        // A backup arms the takeover timer; a plain member can't answer.
        if self.is_steward() && self.timing.sync_resend_anchor.is_none() {
            self.timing.sync_resend_anchor = Some(self.now);
        }
        Ok(())
    }

    /// Apply a validated sync: steward list, protocol bounds, retry ceiling,
    /// peer-score pivot and snapshot, and timing.
    fn adopt_conversation_sync(
        &mut self,
        sync: &ConversationSync,
    ) -> Result<(), ConversationError> {
        let mut list_config = StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?;
        list_config.allow_subset_candidates = sync.allow_subset_candidates;

        let sn = sync.steward_members.len();
        self.steward_list.set_config(list_config);
        self.steward_list.install_list(
            sync.election_epoch,
            &sync.steward_members,
            sn,
            sync.retry_round,
        )?;
        self.steward_list
            .set_max_retries(sync.max_reelection_attempts);
        // store: steward_list (WP3g)
        self.scoring.set_threshold(sync.threshold_peer_score);
        // Before the snapshot: it rebases members off our assumed default onto
        // the group's, which is exactly the set the sparse `peer_scores` omits.
        self.scoring
            .set_default_score(synced_default_peer_score(sync))?;
        let snapshot = ScoreSnapshot {
            diverged: sync
                .peer_scores
                .iter()
                .map(|ps| (ps.member_id.clone(), ps.score))
                .collect(),
        };
        // Record the bootstrap scores, reporting how far each member had
        // already diverged from the default this node would have assumed.
        self.apply_score_snapshot(&snapshot)?;
        // store: scores (WP3g)
        self.config.liveness_criteria_yes = sync.liveness_criteria_yes;
        self.config.pending_update_max_epochs = sync.pending_update_max_epochs;
        if let Some(timing) = &sync.timing {
            self.config.apply_timing(timing);
        }
        // store: meta (WP3g)
        Ok(())
    }
}

/// Encode one `ControlMessage` payload for the router to seal.
pub(crate) fn control_bytes(payload: control_message::Payload) -> Vec<u8> {
    prost::Message::encode_to_vec(&ControlMessage {
        payload: Some(payload),
    })
}

/// Returns `true` when the sync is acceptable for application. Logs the
/// rejection reason on `false`.
///
/// `members` is the adopting node's current member set; ghost stewards
/// (removed since the list was elected) are tolerated as long as at least one
/// listed steward is still present.
///
/// The peer-score parameters are adopted, not judged: where the group puts its
/// starting score, and where it puts the removal line, are the integrator's
/// choices and the library acts on neither. They go through
/// [`ScoringConfig::validate`] — the same rule a locally built config passes at
/// construction — which asks only that the starting score be positive and above
/// the threshold.
fn validate_conversation_sync(
    conversation_id: &str,
    sync: &ConversationSync,
    current_epoch: u64,
    members: &[Vec<u8>],
) -> Result<bool, ConversationError> {
    if sync.election_epoch > current_epoch {
        info!(
            conversation = conversation_id,
            election_epoch = sync.election_epoch,
            current_epoch,
            "conversation sync rejected: election_epoch > current_epoch"
        );
        return Ok(false);
    }

    let members_set = member_set(members);
    let any_present = sync
        .steward_members
        .iter()
        .any(|s| members_set.contains(s.as_slice()));
    // Only accept unsettled members that actually exist, so fake ids can't
    // shrink the pool. Full protection needs every member to agree on the
    // sync's content.
    if !sync
        .unsettled_members
        .iter()
        .all(|u| members_set.contains(u.as_slice()))
    {
        info!(
            conversation = conversation_id,
            "conversation sync rejected: unsettled member is not in the current set"
        );
        return Ok(false);
    }
    let unsettled_set = member_set(&sync.unsettled_members);
    let pool: Vec<Vec<u8>> = members
        .iter()
        .filter(|m| !unsettled_set.contains(m.as_slice()))
        .cloned()
        .collect();
    let ordering_valid = StewardList::validate(
        &sync.steward_members,
        sync.election_epoch,
        conversation_id.as_bytes(),
        &pool,
        &StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?,
        sync.retry_round,
    )?;
    if !(any_present && ordering_valid) {
        info!(
            conversation = conversation_id,
            any_present,
            ordering = ordering_valid,
            "conversation sync rejected: invalid"
        );
        return Ok(false);
    }

    if let Some(timing) = &sync.timing
        && let Some(zero_field) = first_zero_timing_field(timing)
    {
        info!(
            conversation = conversation_id,
            field = zero_field,
            "conversation sync rejected: zero-valued timing field"
        );
        return Ok(false);
    }

    // The same `ScoringConfig::validate` a locally built config passes at
    // construction, so the two ends can't drift apart. A zero
    // `default_peer_score` is proto3's "absent" and resolves to the RFC
    // default, so a sender that never set the field is still joinable.
    if let Err(e) = (ScoringConfig {
        default_score: synced_default_peer_score(sync),
        threshold: sync.threshold_peer_score,
    })
    .validate()
    {
        info!(
            conversation = conversation_id,
            error = %e,
            "conversation sync rejected: peer-score config"
        );
        return Ok(false);
    }
    Ok(true)
}

/// `default_peer_score` the sync asks for. proto3 scalars carry no presence,
/// so a zero is indistinguishable from an unset field and means "whatever the
/// program defaults to" — never a literal starting score of zero, which is not
/// a score to start from.
fn synced_default_peer_score(sync: &ConversationSync) -> i64 {
    if sync.default_peer_score == 0 {
        DEFAULT_PEER_SCORE
    } else {
        sync.default_peer_score
    }
}

/// Name of the first zero-valued field in `timing`, or `None` when every
/// field is non-zero. Zero in any timing field would short-circuit the timer
/// it drives (a consensus timeout firing immediately, an inactivity window
/// that never holds).
fn first_zero_timing_field(timing: &TimingConfig) -> Option<&'static str> {
    if timing.commit_batch_window_ms == 0 {
        Some("commit_batch_window_ms")
    } else if timing.freeze_duration_ms == 0 {
        Some("freeze_duration_ms")
    } else if timing.proposal_expiration_ms == 0 {
        Some("proposal_expiration_ms")
    } else if timing.consensus_timeout_ms == 0 {
        Some("consensus_timeout_ms")
    } else if timing.retry_window_ms == 0 {
        Some("retry_window_ms")
    } else {
        None
    }
}

#[cfg(test)]
mod conversation_sync_tests {
    use super::*;

    fn nonzero_timing() -> TimingConfig {
        TimingConfig {
            commit_batch_window_ms: 60_000,
            freeze_duration_ms: 30_000,
            proposal_expiration_ms: 3_600_000,
            consensus_timeout_ms: 30_000,
            retry_window_ms: 5_000,
            backup_takeover_window_ms: 30_000,
        }
    }

    fn valid_sync_with(threshold: i64) -> ConversationSync {
        ConversationSync {
            steward_members: vec![b"alice".to_vec()],
            election_epoch: 0,
            sn_min: 1,
            sn_max: 5,
            allow_subset_candidates: false,
            peer_scores: vec![],
            timing: Some(nonzero_timing()),
            retry_round: 0,
            max_reelection_attempts: 1,
            liveness_criteria_yes: true,
            threshold_peer_score: threshold,
            pending_update_max_epochs: 3,
            unsettled_members: vec![],
            default_peer_score: 100,
        }
    }

    #[test]
    fn nonzero_timing_passes() {
        assert!(first_zero_timing_field(&nonzero_timing()).is_none());
    }

    /// The synced pair goes through the same `ScoringConfig::validate` a local
    /// config passes at construction: a positive starting score, above the
    /// removal threshold.
    #[test]
    fn validate_accepts_a_sound_peer_score_pair() {
        let sync = valid_sync_with(0);
        assert!(validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap());
    }

    /// A starting score at or below the threshold would admit members already
    /// countable as malicious.
    #[test]
    fn validate_rejects_a_default_not_above_the_threshold() {
        let mut sync = valid_sync_with(0);
        for threshold in [100, 500] {
            sync.threshold_peer_score = threshold;
            assert!(
                !validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap(),
                "default 100 against threshold {threshold} must be rejected"
            );
        }
    }

    /// An unset `default_peer_score` — proto3 gives zero, with no way to tell
    /// it from "never written" — means the program default, so a sender that
    /// predates the field stays joinable.
    #[test]
    fn validate_reads_an_absent_default_as_the_program_default() {
        let mut sync = valid_sync_with(0);
        sync.default_peer_score = 0;
        assert!(validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap());
        assert_eq!(synced_default_peer_score(&sync), DEFAULT_PEER_SCORE);
    }

    /// A negative default was written on purpose and is not a score to start
    /// from.
    #[test]
    fn validate_rejects_a_negative_default() {
        let mut sync = valid_sync_with(0);
        for default in [-1, i64::MIN] {
            sync.default_peer_score = default;
            assert!(
                !validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap(),
                "default_peer_score {default} must be rejected"
            );
        }
    }

    /// The carried delta reconstructs the pool, so a list naming a member the
    /// delta excludes is rejected even though it is internally well-ordered.
    #[test]
    fn validate_verifies_list_against_reconstructed_pool() {
        let members = vec![b"alice".to_vec(), b"bob".to_vec()];
        let mut sync = valid_sync_with(0);
        sync.sn_max = 1;
        // bob is unsettled → excluded → pool = [alice].
        sync.unsettled_members = vec![b"bob".to_vec()];

        // The list correctly derived from the pool is accepted.
        sync.steward_members = vec![b"alice".to_vec()];
        assert!(validate_conversation_sync("g", &sync, 0, &members).unwrap());

        // A list naming the excluded member does not re-derive → rejected.
        sync.steward_members = vec![b"bob".to_vec()];
        assert!(!validate_conversation_sync("g", &sync, 0, &members).unwrap());
    }

    /// A fabricated `unsettled_members` id (not in the current set) is
    /// rejected, so a peer can't shrink the reconstructed pool with ids that
    /// aren't members.
    #[test]
    fn validate_rejects_unsettled_not_in_members() {
        let members = vec![b"alice".to_vec(), b"bob".to_vec()];
        let mut sync = valid_sync_with(0);
        sync.sn_max = 1;
        sync.steward_members = vec![b"alice".to_vec()];
        sync.unsettled_members = vec![b"ghost".to_vec()];
        assert!(!validate_conversation_sync("g", &sync, 0, &members).unwrap());
    }

    #[test]
    fn each_zero_field_is_detected() {
        let cases = [
            (
                "commit_batch_window_ms",
                TimingConfig {
                    commit_batch_window_ms: 0,
                    ..nonzero_timing()
                },
            ),
            (
                "freeze_duration_ms",
                TimingConfig {
                    freeze_duration_ms: 0,
                    ..nonzero_timing()
                },
            ),
            (
                "proposal_expiration_ms",
                TimingConfig {
                    proposal_expiration_ms: 0,
                    ..nonzero_timing()
                },
            ),
            (
                "consensus_timeout_ms",
                TimingConfig {
                    consensus_timeout_ms: 0,
                    ..nonzero_timing()
                },
            ),
            (
                "retry_window_ms",
                TimingConfig {
                    retry_window_ms: 0,
                    ..nonzero_timing()
                },
            ),
        ];
        for (name, timing) in cases {
            assert_eq!(
                first_zero_timing_field(&timing),
                Some(name),
                "expected field {name} to be detected as zero"
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::{
        ConversationConfig,
        engine::{handle::Engine, store::InMemoryStore, types::MemberId},
    };

    /// A member id as the group would issue it: a signature key.
    pub(crate) fn member(name: &str) -> Vec<u8> {
        format!("{name}-signature-key").into_bytes()
    }

    pub(crate) fn id(name: &str) -> MemberId {
        MemberId::from(member(name))
    }

    /// A one-member conversation: the creator is its own steward list, so it
    /// is the epoch steward at every epoch.
    pub(crate) fn creator() -> Engine<InMemoryStore> {
        Engine::create(
            "conv",
            id("alice"),
            1,
            &[id("alice")],
            ConversationConfig::default(),
            InMemoryStore::default(),
        )
        .expect("create")
        .0
    }

    /// A two-member founding group, seen from alice: both members are
    /// settled stewards from `epoch`.
    pub(crate) fn founder() -> Engine<InMemoryStore> {
        Engine::create(
            "conv",
            id("alice"),
            1,
            &[id("alice"), id("bob")],
            ConversationConfig::default(),
            InMemoryStore::default(),
        )
        .expect("create")
        .0
    }

    /// A member seated by a welcome that carried no bootstrap: no steward
    /// list, so it is neither steward nor epoch steward.
    pub(crate) fn joiner() -> Engine<InMemoryStore> {
        Engine::join(
            "conv",
            id("bob"),
            1,
            &[id("alice"), id("bob")],
            &[],
            ConversationConfig::default(),
            InMemoryStore::default(),
        )
        .expect("join")
        .0
    }
}

#[cfg(test)]
mod steward_tests {
    use super::test_support::{creator, founder, id, joiner, member};
    use super::*;
    use crate::{
        ConversationConfig,
        engine::{
            store::InMemoryStore,
            types::{Outbound, Timestamp},
        },
        protos::de_mls::messages::v1::MemberInvite,
    };

    /// The creator is its own steward list, so it answers a sync request by
    /// broadcasting the sync a bootstrap-less joiner can adopt.
    #[test]
    fn epoch_steward_answers_sync_request() {
        let mut engine = creator();
        engine.begin(Timestamp::ZERO);
        engine
            .on_conversation_sync_request(b"joiner")
            .expect("answer request");
        assert!(matches!(
            engine.out.outbound.as_slice(),
            [Outbound::Control(_)]
        ));
    }

    /// A member with no list can't answer — one answerer keeps one sync on the
    /// wire per request.
    #[test]
    fn non_steward_ignores_sync_request_and_arms_nothing() {
        let mut engine = joiner();
        engine.begin(Timestamp::ZERO);
        engine
            .on_conversation_sync_request(b"joiner")
            .expect("ignore request");
        assert!(engine.out.outbound.is_empty());
        assert!(engine.timing.sync_resend_anchor.is_none());
    }

    /// The epoch steward answers reactively, so it never leaves a backup
    /// takeover armed — a stale anchor is cleared.
    #[test]
    fn epoch_steward_request_clears_takeover_anchor() {
        let mut engine = creator();
        engine.begin(Timestamp::ZERO);
        engine.timing.sync_resend_anchor = Some(Timestamp::ZERO);
        engine
            .on_conversation_sync_request(b"joiner")
            .expect("answer request");
        assert!(engine.timing.sync_resend_anchor.is_none());
        assert_eq!(engine.out.outbound.len(), 1);
    }

    /// Sharing is a no-op with no steward list to send yet.
    #[test]
    fn share_conversation_sync_is_noop_without_list() {
        let mut engine = joiner();
        engine.begin(Timestamp::ZERO);
        engine.share_conversation_sync().expect("share sync");
        assert!(engine.out.outbound.is_empty());
    }

    /// The pull side puts one request on the wire per window and counts the
    /// unanswered rounds.
    #[test]
    fn sync_request_drive_asks_once_per_window() {
        let mut engine = joiner();
        engine.begin(Timestamp::ZERO);
        engine.drive_sync_request().expect("first request");
        assert_eq!(engine.out.outbound.len(), 1);
        assert_eq!(engine.timing.unanswered_sync_rounds, 1);

        // Inside the window: no second request.
        engine.drive_sync_request().expect("second request");
        assert_eq!(engine.out.outbound.len(), 1);
        assert_eq!(engine.timing.unanswered_sync_rounds, 1);

        let window = engine.config.backup_takeover_window;
        engine.begin(Timestamp::ZERO + window);
        engine.drive_sync_request().expect("third request");
        assert_eq!(engine.out.outbound.len(), 2);
        assert_eq!(engine.timing.unanswered_sync_rounds, 2);
    }

    /// The bootstrap round-trips: what a steward builds, a listless member
    /// adopts.
    #[test]
    fn bootstrap_round_trips_into_a_joiner() {
        let mut steward = founder();
        steward.begin(Timestamp::ZERO);
        let bytes = steward
            .build_bootstrap()
            .expect("build bootstrap")
            .expect("a list is installed");

        let mut engine = joiner();
        engine.begin(Timestamp::ZERO);
        engine.apply_bootstrap(&bytes).expect("adopt bootstrap");
        assert!(engine.is_synced());
        assert!(engine.out.events.contains(&Event::SyncApplied));
    }

    /// The honest list recomputes from the local members; one drawn off them
    /// does not.
    #[test]
    fn validate_election_list_recomputes_from_local_members() {
        let engine = creator();
        let honest = StewardElectionProposal {
            proposed_stewards: vec![engine.own.clone()],
            election_epoch: engine.epoch,
            retry_round: 0,
        };
        assert!(
            engine.validate_election_list(&honest).unwrap(),
            "the honest list matches the local members"
        );
        let forged = StewardElectionProposal {
            proposed_stewards: vec![member("nobody")],
            election_epoch: engine.epoch,
            retry_round: 0,
        };
        assert!(
            !engine.validate_election_list(&forged).unwrap(),
            "a list off the local members is rejected"
        );
    }

    /// A buffered invite for a seated member and a buffered removal for a
    /// stranger are both inert; a removal for a live member is actionable.
    #[test]
    fn actionable_updates_skip_changes_that_already_landed() {
        let (mut engine, _) = Engine::join(
            "conv",
            id("alice"),
            1,
            &[id("alice"), id("bob")],
            &[],
            ConversationConfig::default(),
            InMemoryStore::default(),
        )
        .expect("join");
        engine.begin(Timestamp::ZERO);

        engine.queues.insert_pending_update(
            ConversationUpdateRequest::member_invite(MemberInvite {
                key_package_bytes: b"kp".to_vec(),
                member_id: member("bob"),
            }),
            engine.epoch,
        );
        engine.queues.insert_pending_update(
            ConversationUpdateRequest::remove_member(member("ghost")),
            engine.epoch,
        );
        assert!(
            engine.actionable_buffered_updates().is_empty(),
            "an invite for a seated member and a removal for a stranger do nothing"
        );

        engine.queues.insert_pending_update(
            ConversationUpdateRequest::remove_member(member("alice")),
            engine.epoch,
        );
        assert_eq!(engine.actionable_buffered_updates().len(), 1);
    }
}
