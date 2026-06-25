//! Steward housekeeping on `Conversation`: list generation/election,
//! pending-update buffering and drain, scoring sync, conversation-sync
//! broadcast.

use std::error::Error as StdError;
use std::sync::Arc;

use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use tracing::{error, info};

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationState, CreatorVote,
    ElectionDecision, ElectionSkip, PeerScoreStorage, member_set,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationSync, ConversationUpdateRequest, PeerScore,
        StewardElectionProposal, TimingConfig, ViolationEvidence, conversation_update_request,
    },
    scoring_member_diff, target_member_id_of,
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

impl<C, Sc> Conversation<C, Sc>
where
    C: ConsensusPlugin,
    Sc: PeerScoreStorage,
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
        // survives freeze rounds so a later steward can retry.
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

    /// Build an encrypted `ConversationSync` payload from the current
    /// steward list + protocol config + peer scores + timing snapshot.
    /// Returns `Ok(None)` when there's no steward list yet. The caller
    /// owns delivery: broadcast it as an [`Outbound`](crate::Outbound)
    /// or feed it into another channel. The post-commit join path bundles
    /// the same payload into [`crate::ConversationEvent::WelcomeReady`].
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
            // Two dedup gates:
            //   - `has_pending_removal` — there's an in-flight ECP for
            //     this target (live consensus session).
            //   - `has_approved_removal` — `RemoveMember(target)` is
            //     already queued in `approved_proposals` waiting for
            //     the next commit. Without this gate, a just-resolved
            //     `ViolationType::SCORE_BELOW_THRESHOLD`ECP (which clears
            //     `pending_removal_targets` on resolve, but leaves the
            //     target in `approved_proposals` with their score still
            //     ≤ threshold) would re-fire a duplicate ECP for the
            //     same target before the RemoveMember commits.
            let self_id: &[u8] = &self_id_arc;
            let mut to_remove: Vec<(Vec<u8>, i64)> = Vec::new();
            for id in self.services.scoring.members_below_threshold()? {
                if id.as_slice() == self_id
                    || self.queues.has_pending_removal(&id)
                    || self.queues.has_approved_removal(&id)
                {
                    continue;
                }
                if let Some(score) = self.services.scoring.score_for(&id)? {
                    to_remove.push((id, score));
                }
            }
            for (id, _) in &to_remove {
                self.queues.insert_pending_removal(id.clone());
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
                self.queues.remove_pending_removal(&target_id);
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

    /// Submit a steward-election proposal. Only the deterministic responsible
    /// proposer actually submits; others no-op, so this is safe to call from
    /// every poll tick without double-proposing.
    ///
    /// `recovery = true` bypasses the list-exhaustion gate and filters
    /// queued-removal targets out of the candidate pool — those entries are
    /// already in `approved_proposals` thanks to
    /// [`crate::apply_consensus_result`], so `has_approved_removal`
    /// catches them without an explicit exclude.
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

            // Build the candidate pool: settled MLS members minus queued
            // removals (recovery only — non-recovery elections trust the
            // current MLS roster). Unsettled (just-joined) members are excluded
            // — they may not have attached MLS and can't serve as stewards yet.
            let candidate_pool: Vec<Vec<u8>> = mls_members
                .iter()
                .filter(|m| self.queues.is_settled(m, epoch))
                .filter(|m| !(recovery && self.queues.has_approved_removal(m)))
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
                    if skip == ElectionSkip::NoEligibleCandidates {
                        info!(
                            conversation = %self.conversation_id,
                            "skipping election: {skip}"
                        );
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

    /// Layer 3 escalation: file a `Deadlock` ECP after re-election retries
    /// exhaust. Only the deterministic responsible proposer submits;
    /// others no-op. On YES the ECP opens `recovery_mode`.
    pub(crate) fn initiate_deadlock_ecp<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let (is_authorized, self_id, epoch, conversation_id) = {
            let mls = self.mls();
            let mls_members = mls.members()?;
            let epoch = mls.current_epoch()?;
            let self_id: &[u8] = &self.self_member_id;
            // Deadlock proposer = election proposer with the stricter
            // predicate (MLS-present and not queued for removal).
            let mls_set: std::collections::HashSet<&[u8]> =
                mls_members.iter().map(Vec::as_slice).collect();
            let conversation_ref = &self.queues;
            let authorized = self
                .services
                .steward_list
                .election_proposer(|c: &[u8]| {
                    mls_set.contains(c) && !conversation_ref.has_approved_removal(c)
                })
                .is_some_and(|proposer| proposer == self_id);
            (
                authorized,
                Arc::clone(&self.self_member_id),
                epoch,
                self.conversation_id.clone(),
            )
        };

        if !is_authorized {
            return Ok(());
        }

        let request = ViolationEvidence::deadlock(epoch)
            .with_creator(self_id.to_vec())
            .into_update_request()?;

        info!(
            conversation = %conversation_id,
            epoch, "initiating Deadlock ECP"
        );

        // Bundle YES — the proposer's observation that the deadlock is
        // real is their vote.
        self.initiate_proposal(provider, request, CreatorVote::Yes, signer)?;
        Ok(())
    }
}
