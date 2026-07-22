//! Steward housekeeping on `Conversation`: list generation/election,
//! pending-update buffering and drain, scoring sync, conversation-sync
//! broadcast.

use std::error::Error as StdError;
use std::sync::Arc;

use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use tracing::{error, info};

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationEvent, ConversationState,
    CreatorVote, ElectionDecision, ElectionSkip, PeerScoreStorage, WallClock, member_set,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationSync, ConversationUpdateRequest, PeerScore,
        StewardElectionProposal, TimingConfig, ViolationEvidence, conversation_update_request,
    },
    scoring_member_diff, target_member_id_of, wall_clock::WallClockExt,
};

/// Outcome of reconciling the steward list to the current epoch — see
/// [`Conversation::reconcile_steward_list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StewardListReconcile {
    /// List already covered the epoch, or the settled members were installed
    /// locally and deterministically. No consensus round needed.
    Settled,
    /// The settled members exceed `sn_max`, so the list is a genuine subset
    /// peers must agree on: the caller runs the voted election.
    NeedsElection,
}

impl<Cp, Sc, Wc> Conversation<Cp, Sc, Wc>
where
    Cp: ConsensusPlugin,
    Sc: PeerScoreStorage,
    Wc: WallClock,
{
    // ── Public API ───────────────────────────────────────────────────

    /// Add any MLS members not yet tracked in scoring, and drop scored
    /// entries for identities no longer in MLS. Diffing is delegated to
    /// [`scoring_member_diff`]; this method only applies the diff.
    pub(crate) fn sync_scoring_members(
        &mut self,
        mls_members: &[Vec<u8>],
    ) -> Result<(), ConversationError> {
        let scored: Vec<Vec<u8>> = self
            .services
            .scoring
            .all_members_with_scores()?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let diff = scoring_member_diff(&scored, mls_members);
        for member_id in &diff.to_add {
            self.services.scoring.add_member(member_id)?;
        }
        for member_id in &diff.to_remove {
            self.services.scoring.remove_member(member_id)?;
        }
        Ok(())
    }

    /// Reconcile the list after an epoch advance, opening a voted election when
    /// it's needed. Election-init failures are logged, not surfaced — the
    /// conversation may legitimately reject a new proposal right now.
    pub(crate) fn steward_list_housekeeping<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let reconcile = self.reconcile_steward_list()?;
        if reconcile == StewardListReconcile::NeedsElection
            && let Err(e) = self.initiate_steward_election(provider, false, signer)
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
        let (current_epoch, members) = {
            let mls = self.mls();
            (mls.current_epoch()?, mls.members()?)
        };
        if !self.services.steward_list.is_exhausted(current_epoch) {
            return Ok(StewardListReconcile::Settled);
        }
        // Stewards are settled members only — a just-joined member can't commit
        // or vote yet, so it's excluded until the next epoch.
        let settled = self.queues.settled_members(&members, current_epoch);
        if self.services.steward_list.election_required(settled.len()) {
            return Ok(StewardListReconcile::NeedsElection);
        }
        let sn = self
            .services
            .steward_list
            .config()
            .compute_list_size(settled.len());
        self.services
            .steward_list
            .install_list(current_epoch, &settled, sn, 0)?;
        Ok(StewardListReconcile::Settled)
    }

    /// Handle an incoming membership update (KP-derived `MemberInvite` or
    /// `RemoveMember`): buffer it so every member has a durable record, then
    /// promote it to a voting proposal if this node is the current epoch
    /// steward and the conversation accepts new proposals.
    pub(crate) fn handle_incoming_update_request<Pr>(
        &mut self,
        provider: &Pr,
        request: ConversationUpdateRequest,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let state = self.current_state();
        // Defensive — core only emits membership changes here.
        if target_member_id_of(&request).is_none() {
            return Ok(());
        }
        let (members, current_epoch) = {
            let mls = self.mls();
            (mls.members()?, mls.current_epoch()?)
        };

        let inserted = self
            .queues
            .insert_pending_update(request.clone(), current_epoch);

        // Only the epoch steward proposes immediately. The buffer
        // survives commit rounds so a later steward can retry.
        let is_epoch_steward = {
            let eligible = self.queues.steward_eligibility(&members);
            self.services
                .steward_list
                .epoch_steward(current_epoch, &eligible)
                .is_some_and(|es| es == &*self.self_member_id)
        };
        let should_propose = is_epoch_steward && state == ConversationState::Working;

        info!(
            conversation = %self.conversation_id,
            epoch = current_epoch,
            inserted,
            buffer_total = self.queues.pending_update_count(),
            is_epoch_steward,
            state = %state,
            propose = should_propose,
            "update request buffered"
        );

        if should_propose {
            // Steward auto-propose: the steward forwards peer intent and
            // still holds a judgement call, so we broadcast unbundled and
            // let the vote request drive the steward's vote like any other member.
            // `check_proposal_allowed` may still reject (active emergency
            // etc.) — leave the entry in the buffer for next rotation.
            if let Err(e) = self.initiate_proposal(provider, request, CreatorVote::Deferred, signer)
            {
                info!(conversation = %self.conversation_id, error = %e, "proposal deferred");
            }
        }
        Ok(())
    }

    /// Drop Add entries whose target is now a member and Remove entries
    /// whose target is now gone, then expire entries older than
    /// `pending_update_max_epochs`.
    pub(crate) fn prune_pending_updates_after_commit(&mut self) -> Result<(), ConversationError> {
        let (current_epoch, members, max_age) = {
            let mls = self.mls();
            (
                mls.current_epoch()?,
                mls.members()?,
                self.config.pending_update_max_epochs,
            )
        };

        let before = self.queues.pending_update_count();
        self.queues.prune_pending_updates_for_members(&members);
        let expired = self.queues.expire_pending_updates(current_epoch, max_age);
        let after = self.queues.pending_update_count();
        if before != after {
            info!(
                conversation = %self.conversation_id,
                before,
                after,
                expired = expired.len(),
                "pruned pending updates after commit"
            );
        }
        Ok(())
    }

    /// On epoch advance, the new live epoch steward drains the pending-update
    /// buffer into voting proposals. A backup steward reaches the same drain
    /// from `poll` once the recovery window passes (see
    /// [`Self::drain_buffered_updates`]).
    pub(crate) fn process_buffered_updates<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if !self.is_epoch_steward()? {
            return Ok(());
        }
        self.drain_buffered_updates(provider, signer)
    }

    /// Propose every actionable buffered membership update now. de-mls no longer
    /// times this — the app calls it when its liveness policy decides to act:
    /// the epoch steward drains immediately, a backup takes over a silent
    /// primary. Returns `Ok(true)` if a drain ran, `Ok(false)` when there is
    /// nothing to do — not in `Working`, approved work already pending, not a
    /// steward, or the buffer holds nothing actionable. Read
    /// [`Self::pending_buffered_updates`] to decide when to call it.
    pub fn propose_buffered_updates<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<bool, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if self.current_state() != ConversationState::Working
            || self.queues.approved_proposals_count() > 0
            || !self.is_steward()
            || self.actionable_buffered_updates()?.is_empty()
        {
            return Ok(false);
        }
        self.drain_buffered_updates(provider, signer)?;
        Ok(true)
    }

    /// Buffered membership updates still needing a proposal: not already
    /// covered by a live (voting or approved) proposal, and still valid — an
    /// Add for a non-member or a Remove for a current member.
    pub(crate) fn actionable_buffered_updates(
        &self,
    ) -> Result<Vec<ConversationUpdateRequest>, ConversationError> {
        let members = self.mls().members()?;
        let members_set = member_set(&members);
        let active = self.queues.active_proposal_targets();
        Ok(self
            .queues
            .pending_updates()
            .iter()
            .filter(|(id, _)| !active.contains(id.as_slice()))
            .filter(|(id, p)| {
                // Drop Add for already-member and Remove for non-member.
                let is_member = members_set.contains(id.as_slice());
                match p.request.payload.as_ref() {
                    Some(conversation_update_request::Payload::MemberInvite(_)) => !is_member,
                    Some(conversation_update_request::Payload::RemoveMember(_)) => is_member,
                    _ => false,
                }
            })
            .map(|(_, p)| p.request.clone())
            .collect())
    }

    /// Promote every actionable buffered update to a voting proposal
    /// (`CreatorVote::Deferred` — the proposer relays peer intent, it doesn't
    /// endorse). Callers gate *who* drains: the epoch steward immediately via
    /// [`Self::process_buffered_updates`], a backup after the recovery window
    /// via `poll`. This just performs the drain.
    pub(crate) fn drain_buffered_updates<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let to_propose = self.actionable_buffered_updates()?;
        if to_propose.is_empty() {
            return Ok(());
        }
        info!(
            conversation = %self.conversation_id,
            count = to_propose.len(),
            "promoting buffered updates to proposals"
        );
        for request in to_propose {
            if let Err(e) = self.initiate_proposal(provider, request, CreatorVote::Deferred, signer)
            {
                info!(
                    conversation = %self.conversation_id,
                    error = %e,
                    "buffered proposal deferred"
                );
            }
        }
        Ok(())
    }

    /// Build the encrypted `ConversationSync` payload from the current steward
    /// list, config, peer scores, and timing. Returns `Ok(None)` when there's
    /// no list yet; the caller owns delivery.
    pub(crate) fn build_conversation_sync_payload<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<Option<Vec<u8>>, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        // Sparse snapshot — only members whose score has diverged
        // from `default_score`. Joiners init every member at default
        // via membership sync before applying the snapshot, so
        // missing entries imply default. Saves wire size at scale
        // (Waku message budget concern past ~1k members).
        let scores: Vec<PeerScore> = self
            .services
            .scoring
            .snapshot()?
            .diverged
            .into_iter()
            .map(|(id, score)| PeerScore {
                member_id: id,
                score,
            })
            .collect();

        let list = match self.services.steward_list.current_list() {
            Some(l) => l,
            None => return Ok(None),
        };

        let timing = TimingConfig::from(&self.config);

        // Filter ghosts and queued-removal targets so joiners don't
        // inherit stewards they would have to walk past on the very
        // first epoch.
        let mls_members = self.mls().members()?;
        let steward_members = {
            let eligible = self.queues.steward_eligibility(&mls_members);
            self.services.steward_list.steward_members(&eligible)
        };

        // `retry_round` is the seed that produced the *stored* list —
        // a frozen tag on `StewardList`, not the plug-in's dynamic
        // counter for the next attempt (which resets to 0 on accept).
        // Joiners re-derive the ordering from this seed.
        let sync = ConversationSync {
            steward_members,
            election_epoch: list.election_epoch(),
            sn_min: list.config().sn_min as u32,
            sn_max: list.config().sn_max as u32,
            allow_subset_candidates: self.services.steward_list.config().allow_subset_candidates,
            peer_scores: scores,
            timing: Some(timing),
            retry_round: list.retry_round(),
            max_reelection_attempts: self.services.steward_list.max_retries(),
            liveness_criteria_yes: self.config.liveness_criteria_yes,
            threshold_peer_score: self.services.scoring.threshold(),
            pending_update_max_epochs: self.config.pending_update_max_epochs,
        };

        let app_msg: AppMessage = sync.into();
        Ok(Some(
            self.mls_mut().build_message(provider, signer, &app_msg)?,
        ))
    }

    /// Broadcast the current `ConversationSync` so a bootstrap-less joiner can
    /// adopt it — called by the epoch steward on a request, or by the integrator
    /// directly. No-op when there's no list to share yet.
    pub fn share_conversation_sync<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if let Some(payload) = self.build_conversation_sync_payload(provider, signer)? {
            self.broadcast(payload);
        }
        Ok(())
    }

    /// Steward-only: file `ScoreBelowThreshold` ECPs for any member whose
    /// score fell at or below the removal threshold. Skips self and any
    /// target already covered by a pending removal.
    pub(crate) fn check_and_initiate_score_removals<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        // Reactive entry: callers chain into this after a scoring apply
        // emitted a downward cross, so we expect at least one tracked
        // member to be at-or-below threshold. The scan is the source of
        // truth — events just trigger the look.
        let (epoch, to_remove, self_id_arc, conversation_id) = {
            let epoch = self.mls().current_epoch()?;
            let self_id_arc = Arc::clone(&self.self_member_id);
            let is_steward = self.services.steward_list.is_steward(&self_id_arc);
            if !is_steward {
                return Ok(());
            }
            // Skip a target that already has a removal in flight or approved.
            // The in-flight index now includes score-removal ECPs, so this one
            // check covers both a live ECP session and a queued RemoveMember —
            // the target stays covered across the ECP's whole lifecycle, so no
            // duplicate is filed.
            let active = self.queues.active_proposal_targets();
            let self_id: &[u8] = &self_id_arc;
            let mut to_remove: Vec<(Vec<u8>, i64)> = Vec::new();
            for id in self.services.scoring.members_below_threshold()? {
                if id.as_slice() == self_id || active.contains(id.as_slice()) {
                    continue;
                }
                if let Some(score) = self.services.scoring.score_for(&id)? {
                    to_remove.push((id, score));
                }
            }
            (epoch, to_remove, self_id_arc, self.conversation_id.clone())
        };

        for (target_id, current_score) in to_remove {
            let evidence =
                ViolationEvidence::score_below_threshold(target_id.clone(), epoch, current_score)
                    .with_creator(self_id_arc.to_vec());
            let request = evidence.into_update_request()?;

            info!(
                conversation = %conversation_id,
                target = ?target_id,
                score = current_score,
                "initiating SCORE_BELOW_THRESHOLD removal"
            );
            // `ViolationType::SCORE_BELOW_THRESHOLD`is self-executing: threshold crossed ⇒
            // member must be removed. The steward's vote is YES by
            // protocol, so we bundle it at submit and skip the vote request.
            if let Err(e) = self.initiate_proposal(provider, request, CreatorVote::Yes, signer) {
                error!(
                    conversation = %conversation_id,
                    target = ?target_id,
                    error = %e,
                    "SCORE_BELOW_THRESHOLD vote failed to start"
                );
            }
        }

        Ok(())
    }

    // ── Crate-internal ───────────────────────────────────────────────

    /// Recompute the steward list from our own settled roster and check
    /// `proposed` matches it, so a proposer can't slip in a biased list — we
    /// trust our own membership view, not the payload. Using the current roster
    /// is safe: no commit can land mid-election, so it hasn't moved since the
    /// proposer built the list.
    pub(crate) fn validate_election_list(
        &self,
        election: &StewardElectionProposal,
    ) -> Result<bool, ConversationError> {
        let epoch = election.election_epoch;
        let members = self.mls().members()?;
        let pool: Vec<Vec<u8>> = members
            .iter()
            .filter(|m| self.queues.is_settled(m, epoch))
            .cloned()
            .collect();
        self.services.steward_list.validate_proposed(
            &election.proposed_stewards,
            epoch,
            &pool,
            election.retry_round,
        )
    }

    /// Submit a steward-election proposal. Only the deterministic responsible
    /// proposer actually submits; others no-op, so this is safe to call every
    /// poll tick without double-proposing. `recovery = true` bypasses the
    /// list-exhaustion gate to force an election.
    pub(crate) fn initiate_steward_election<Pr>(
        &mut self,
        provider: &Pr,
        recovery: bool,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let (proposed_stewards, election_epoch, retry_round, conversation_id) = {
            let mls = self.mls();
            let epoch = mls.current_epoch()?;
            let mls_members = mls.members()?;
            let self_member_id: &[u8] = &self.self_member_id;

            // `has_election_in_flight` is a proposal-queue check, not a
            // steward-list one — gated here, before the plug-in call.
            if self.queues.has_election_in_flight() {
                return Ok(());
            }

            // Candidate pool: settled MLS members only — the list is a pure
            // rotation of that roster by `retry_round`, so every member derives
            // the same one. Unsettled (just-joined) members are excluded: they
            // may not have attached MLS yet.
            let candidate_pool: Vec<Vec<u8>> = mls_members
                .iter()
                .filter(|m| self.queues.is_settled(m, epoch))
                .cloned()
                .collect();
            let pool_set: std::collections::HashSet<&[u8]> =
                candidate_pool.iter().map(Vec::as_slice).collect();
            let eligible = |c: &[u8]| pool_set.contains(c);

            match self.services.steward_list.propose_election(
                epoch,
                &candidate_pool,
                self_member_id,
                eligible,
                recovery,
            )? {
                ElectionDecision::Skip(skip) => {
                    match skip {
                        // Routine outcome of every epoch-advance housekeeping
                        // call — not worth a log line.
                        ElectionSkip::NotExhausted => {}
                        ElectionSkip::NotResponsibleProposer => {
                            let proposer = self.services.steward_list.responsible_proposer(
                                self.services.steward_list.next_election_round(),
                                &candidate_pool,
                                eligible,
                            );
                            info!(
                                conversation = %self.conversation_id,
                                proposer = ?proposer,
                                retry_round = self.services.steward_list.next_election_round(),
                                "skipping election: {skip}"
                            );
                        }
                        ElectionSkip::NoEligibleCandidates => {
                            info!(
                                conversation = %self.conversation_id,
                                "skipping election: {skip}"
                            );
                        }
                    }
                    return Ok(());
                }
                ElectionDecision::Proposed {
                    proposed_stewards,
                    election_epoch,
                    retry_round,
                } => (
                    proposed_stewards,
                    election_epoch,
                    retry_round,
                    self.conversation_id.clone(),
                ),
            }
        };

        let stewards_len = proposed_stewards.len();
        let request = ConversationUpdateRequest::steward_election(StewardElectionProposal {
            proposed_stewards,
            election_epoch,
            retry_round,
        });

        info!(
            conversation = %conversation_id,
            epoch = election_epoch,
            retry_round,
            stewards = stewards_len,
            recovery,
            "initiating steward election"
        );

        // Elections are conversation-wide decisions — broadcast unbundled
        // so the responsible proposer still votes via the vote request.
        self.initiate_proposal(provider, request, CreatorVote::Deferred, signer)?;

        Ok(())
    }

    /// Raise the Layer-3 deadlock signal: file a `Deadlock` ECP to open recovery
    /// when the group is stuck with no steward committing. It only *proposes* —
    /// the ECP needs ⌈2n/3⌉ consensus, so one member can't force it. On YES the
    /// group enters recovery ([`crate::ConversationEvent::RecoveryModeOpened`]),
    /// relaxing the steward gate. No-op while already in recovery.
    pub fn request_recovery<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if self.is_in_recovery_mode() {
            return Ok(());
        }
        let (self_id, epoch) = {
            let mls = self.mls();
            (Arc::clone(&self.self_member_id), mls.current_epoch()?)
        };
        let request = ViolationEvidence::deadlock(epoch)
            .with_creator(self_id.to_vec())
            .into_update_request()?;
        info!(conversation = %self.conversation_id, epoch, "initiating Deadlock ECP");
        // Bundled YES: filing it is this member's own vote that the deadlock is real.
        self.initiate_proposal(provider, request, CreatorVote::Yes, signer)?;
        Ok(())
    }

    /// Advance the election retry ladder after a failed round — an election
    /// rejected by vote, or a silent round where no proposal was filed at all.
    /// Bumps the retry round (which re-seeds the steward list and hands proposer
    /// authority to the next candidate) and re-runs the election.
    ///
    /// de-mls owns this because the retry round re-seeds the shared list — every
    /// member must advance in lockstep or they elect different lists. `poll`
    /// drives it on the round window; the consensus layer drives it on a
    /// rejection. On exhaustion it emits
    /// [`ConversationEvent::ReelectionExhausted`] and stops — opening Layer-3
    /// recovery is the integrator's liveness call, not an auto-escalation.
    pub(crate) fn advance_election_retry<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.services.steward_list.bump_retry();
        let round = self.services.steward_list.next_election_round();
        let max = self.services.steward_list.max_retries();
        if round > max {
            info!(
                conversation = %self.conversation_id,
                round, max, "election retries exhausted"
            );
            // Stop the round timer and surface it; recovery is the app's call.
            self.timing.phase_timer.clear();
            self.emit_event(ConversationEvent::ReelectionExhausted);
            return Ok(());
        }
        // Re-anchor the round window so the next silent round is timed afresh.
        self.timing.phase_timer.start(self.clock.timestamp());
        info!(
            conversation = %self.conversation_id,
            round, max, "election round failed, retrying"
        );
        if let Err(e) = self.initiate_steward_election(provider, true, signer) {
            info!(conversation = %self.conversation_id, error = %e, "election retry deferred");
        }
        Ok(())
    }
}
