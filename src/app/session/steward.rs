//! Post-epoch-advance steward housekeeping on `SessionRunner`: list
//! generation/election, pending-update drain, scoring sync, conversation-sync
//! broadcast.
//!
//! Methods that file new proposals (`initiate_steward_election`,
//! `initiate_deadlock_ecp`, `process_buffered_updates`,
//! `check_and_initiate_score_removals`) are associated functions taking
//! `Arc<RwLock<SessionRunner>>` so they can call
//! [`SessionRunner::initiate_proposal`] which itself spawns a background
//! task. The rest are `&self` / `&mut self` methods on the runner.

use std::sync::{Arc, RwLock};

use tracing::{error, info};

use crate::{
    app::{CreatorVote, LockExt, SessionRunner, UserError},
    core::{
        ConsensusPlugin, ConversationPluginsFactory, ElectionDecision, PeerScoringPlugin,
        StewardListPlugin, member_set, scoring_member_diff, target_member_id_of,
    },
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationSync, ConversationUpdateRequest, PeerScore,
        StewardElectionProposal, TimingConfig, ViolationEvidence, conversation_update_request,
    },
};

/// Outcome of reconciling the steward list to the current epoch — see
/// [`SessionRunner::reconcile_steward_list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StewardListReconcile {
    /// List already covered the epoch, or a small group's list was
    /// reinstalled deterministically. No consensus round needed.
    Settled,
    /// The group is large enough (`members > sn_max`) that the list is a
    /// genuine subset peers must agree: the caller runs the voted election.
    NeedsElection,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    // ── Public API ───────────────────────────────────────────────────

    /// Add any MLS members not yet tracked in scoring, and drop scored
    /// entries for identities no longer in MLS. Diffing is delegated to
    /// [`scoring_member_diff`]; this method only applies the diff.
    pub fn sync_scoring_members(&mut self, mls_members: &[Vec<u8>]) {
        let scored: Vec<Vec<u8>> = self
            .conversation
            .scoring
            .all_members_with_scores()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let diff = scoring_member_diff(&scored, mls_members);
        for member_id in &diff.to_add {
            // Under the standard config (`default > threshold`) this
            // returns false; an exotic config could surface a fresh member
            // as below-threshold, but the score-removal chain doesn't fire
            // on membership-sync ticks today.
            let _ = self.conversation.scoring.add_member(member_id);
        }
        for member_id in &diff.to_remove {
            self.conversation.scoring.remove_member(member_id);
        }
    }

    /// Post-epoch-advance reconcile: bring the steward list to the new epoch.
    /// Small groups regenerate locally (no consensus); only a large group,
    /// whose list is a genuine subset, opens a voted election. Election-init
    /// failures are logged, not surfaced — conversation state may legitimately
    /// reject a new proposal right now.
    pub fn steward_list_housekeeping(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let reconcile = arc.write_or_err("session")?.reconcile_steward_list()?;
        if reconcile == StewardListReconcile::NeedsElection
            && let Err(e) = Self::initiate_steward_election(arc, false)
        {
            let conv_name = arc.read_or_err("session")?.conversation_id.clone();
            info!(conversation = %conv_name, error = %e, "election initiation deferred");
        }
        Ok(())
    }

    /// Reconcile the steward list to the current MLS epoch. No-op if the
    /// installed list still covers this epoch. Otherwise the group size
    /// decides: small (`members <= sn_max`, every member is a steward) →
    /// install the deterministic list locally, no vote; large (`members >
    /// sn_max`) → return [`StewardListReconcile::NeedsElection`] for the
    /// caller to run a voted election. The local install is the same list a
    /// successful election would yield: every node — the committer before it
    /// builds the `ConversationSync`, and each peer on its own commit-merge —
    /// computes the identical list.
    pub(crate) fn reconcile_steward_list(&mut self) -> Result<StewardListReconcile, UserError> {
        let (current_epoch, members) = {
            let mls = self.conversation.expect_mls()?;
            (mls.current_epoch()?, mls.members()?)
        };
        if !self.conversation.steward_list.is_exhausted(current_epoch) {
            return Ok(StewardListReconcile::Settled);
        }
        // Trigger a voted election only when *settled* members exceed sn_max. A
        // member added by this epoch's commit may not have attached MLS yet, so
        // electing on their account would fire a vote they'd miss, diverging the
        // steward list. The deterministic local install below is computed
        // identically by every node, so it can safely include just-joined
        // members (they receive the same list via ConversationSync).
        let settled_count = self
            .conversation
            .conversation
            .settled_members(&members, current_epoch)
            .len();
        if self
            .conversation
            .steward_list
            .election_required(settled_count)
        {
            return Ok(StewardListReconcile::NeedsElection);
        }
        let sn = self
            .conversation
            .steward_list
            .config()
            .compute_list_size(members.len());
        let _events =
            self.conversation
                .steward_list
                .install_list(current_epoch, &members, sn, 0)?;
        Ok(StewardListReconcile::Settled)
    }

    /// Drop Add entries whose target is now a member and Remove entries
    /// whose target is now gone, then expire entries older than
    /// `pending_update_max_epochs`.
    pub fn prune_pending_updates_after_commit(&mut self) -> Result<(), UserError> {
        let (current_epoch, members, max_age) = {
            let Some(mls) = self.conversation.mls() else {
                return Ok(());
            };
            (
                mls.current_epoch()?,
                mls.members()?,
                self.conversation.config.pending_update_max_epochs,
            )
        };

        let before = self.conversation.conversation.pending_update_count();
        self.conversation
            .conversation
            .prune_pending_updates_for_members(&members);
        let expired = self
            .conversation
            .conversation
            .expire_pending_updates(current_epoch, max_age);
        let after = self.conversation.conversation.pending_update_count();
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
    /// buffer into voting proposals. Skips entries already covered by the
    /// current voting/approved queues so we don't double-propose.
    pub fn process_buffered_updates(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let (current_epoch, to_propose, conversation_id): (
            u64,
            Vec<ConversationUpdateRequest>,
            String,
        ) = {
            let s = arc.read_or_err("session")?;

            let (current_epoch, members) = match s.conversation.mls() {
                Some(mls) => (mls.current_epoch()?, mls.members()?),
                None => (0, Vec::new()),
            };
            let self_member_id: &[u8] = &s.self_member_id;
            let eligible = s.conversation.conversation.steward_eligibility(&members);
            let is_live = s
                .conversation
                .steward_list
                .epoch_steward(current_epoch, &eligible)
                .is_some_and(|es| es == self_member_id);
            if !is_live {
                return Ok(());
            }

            // Collect buffered updates whose target isn't already in an
            // active proposal queue. Both the voting and approved queues
            // live on `ConversationQueues`.
            let approved = s.conversation.conversation.approved_proposals();
            let approved_targets: std::collections::HashSet<&[u8]> =
                approved.values().filter_map(target_member_id_of).collect();
            let members_set = member_set(&members);

            let to_propose: Vec<ConversationUpdateRequest> = s
                .conversation
                .conversation
                .pending_updates()
                .iter()
                .filter(|(id, _)| !approved_targets.contains(id.as_slice()))
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
                .collect();
            (current_epoch, to_propose, s.conversation_id.clone())
        };

        if to_propose.is_empty() {
            return Ok(());
        }

        info!(
            conversation = %conversation_id,
            epoch = current_epoch,
            count = to_propose.len(),
            "promoting buffered updates to proposals"
        );

        // Buffered updates inherit the same banner path as fresh
        // steward-auto-propose — the steward still decides per proposal.
        for request in to_propose {
            if let Err(e) = Self::initiate_proposal(arc, request, CreatorVote::Deferred) {
                info!(
                    conversation = %conversation_id,
                    error = %e,
                    "buffered proposal deferred"
                );
            }
        }
        Ok(())
    }

    /// Build an encrypted `ConversationSync` packet from the current
    /// steward list + protocol config + peer scores + timing snapshot.
    /// Returns `Ok(None)` when there's no steward list yet. The caller
    /// owns publish: hand the packet to the transport for a
    /// re-broadcast, or feed the payload into another channel. The
    /// post-commit join path bundles the same payload into
    /// [`crate::core::SessionEvent::WelcomeReady`].
    pub fn build_conversation_sync_packet(
        &mut self,
    ) -> Result<Option<crate::ds::OutboundPacket>, UserError> {
        // Sparse snapshot — only members whose score has diverged
        // from `default_score`. Joiners init every member at default
        // via membership sync before applying the snapshot, so
        // missing entries imply default. Saves wire size at scale
        // (Waku message budget concern past ~1k members).
        let scores: Vec<PeerScore> = self
            .conversation
            .scoring
            .snapshot()
            .diverged
            .into_iter()
            .map(|(id, score)| PeerScore {
                member_id: id,
                score,
            })
            .collect();

        let list = match self.conversation.steward_list.current_list() {
            Some(l) => l,
            None => return Ok(None),
        };

        let timing = TimingConfig::from(&self.conversation.config);

        // Filter ghosts and queued-removal targets so joiners don't
        // inherit stewards they would have to walk past on the very
        // first epoch.
        let mls_members = self.conversation.expect_mls()?.members()?;
        let steward_members = {
            let eligible = self
                .conversation
                .conversation
                .steward_eligibility(&mls_members);
            self.conversation.steward_list.steward_members(&eligible)
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
            allow_subset_candidates: self
                .conversation
                .steward_list
                .config()
                .allow_subset_candidates,
            peer_scores: scores,
            timing: Some(timing),
            retry_round: list.retry_round(),
            max_reelection_attempts: self.conversation.steward_list.max_retries(),
            liveness_criteria_yes: self.conversation.config.liveness_criteria_yes,
            threshold_peer_score: self.conversation.scoring.threshold(),
            pending_update_max_epochs: self.conversation.config.pending_update_max_epochs,
        };

        let app_msg: AppMessage = sync.into();
        let app_id = Arc::clone(&self.app_id);
        Ok(Some(
            self.conversation
                .expect_mls_mut()?
                .build_message(&app_msg, &app_id)?,
        ))
    }

    /// Steward-only: file `ScoreBelowThreshold` ECPs for any member whose
    /// score fell at or below the removal threshold. Skips self and any
    /// target already covered by a pending removal.
    pub fn check_and_initiate_score_removals(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        // Reactive entry: callers chain into this after a scoring apply
        // emitted a downward cross, so we expect at least one tracked
        // member to be at-or-below threshold. The scan is the source of
        // truth — events just trigger the look.
        let (epoch, to_remove, self_id_arc, conversation_id) = {
            let mut s = arc.write_or_err("session")?;
            let epoch = s.conversation.expect_mls()?.current_epoch()?;
            let self_id_arc = Arc::clone(&s.self_member_id);
            let is_steward = s.conversation.steward_list.is_steward(&self_id_arc);
            if !is_steward {
                return Ok(());
            }
            // Two dedup gates:
            //   - `has_pending_removal` — there's an in-flight ECP for
            //     this target (live consensus session).
            //   - `has_approved_removal` — `RemoveMember(target)` is
            //     already queued in `approved_proposals` waiting for
            //     the next commit. Without this gate, a just-resolved
            //     SCORE_BELOW_THRESHOLD ECP (which clears
            //     `pending_removal_targets` on resolve, but leaves the
            //     target in `approved_proposals` with their score still
            //     ≤ threshold) would re-fire a duplicate ECP for the
            //     same target before the RemoveMember commits.
            let self_id: &[u8] = &self_id_arc;
            let to_remove: Vec<(Vec<u8>, i64)> = s
                .conversation
                .scoring
                .members_below_threshold()
                .into_iter()
                .filter(|id| id.as_slice() != self_id)
                .filter(|id| !s.conversation.conversation.has_pending_removal(id))
                .filter(|id| !s.conversation.conversation.has_approved_removal(id))
                .filter_map(|id| {
                    let score = s.conversation.scoring.score_for(&id)?;
                    Some((id, score))
                })
                .collect();
            for (id, _) in &to_remove {
                s.conversation
                    .conversation
                    .insert_pending_removal(id.clone());
            }
            (epoch, to_remove, self_id_arc, s.conversation_id.clone())
        };

        // Submit proposals without holding the lock.
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
            // SCORE_BELOW_THRESHOLD is self-executing: threshold crossed ⇒
            // member must be removed. The steward's vote is YES by
            // protocol, so we bundle it at submit and skip the banner.
            if let Err(e) = Self::initiate_proposal(arc, request, CreatorVote::Yes) {
                arc.write_or_err("session")?
                    .conversation
                    .conversation
                    .remove_pending_removal(&target_id);
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
    /// [`crate::core::apply_consensus_result`], so `has_approved_removal`
    /// catches them without an explicit exclude.
    pub fn initiate_steward_election(
        arc: &Arc<RwLock<Self>>,
        recovery: bool,
    ) -> Result<(), UserError> {
        let (proposed_stewards, election_epoch, retry_round, conversation_id) = {
            let s = arc.read_or_err("session")?;
            let mls = s.conversation.expect_mls()?;
            let epoch = mls.current_epoch()?;
            let mls_members = mls.members()?;
            let self_member_id: &[u8] = &s.self_member_id;

            // `has_election_in_flight` is a proposal-queue check, not a
            // steward-list one — gated here, before the plug-in call.
            if s.conversation.conversation.has_election_in_flight() {
                return Ok(());
            }

            // Build the candidate pool: settled MLS members minus queued
            // removals (recovery only — non-recovery elections trust the
            // current MLS roster). Unsettled (just-joined) members are excluded
            // — they may not have attached MLS and can't serve as stewards yet.
            let candidate_pool: Vec<Vec<u8>> = mls_members
                .iter()
                .filter(|m| s.conversation.conversation.is_settled(m, epoch))
                .filter(|m| !(recovery && s.conversation.conversation.has_approved_removal(m)))
                .cloned()
                .collect();
            let pool_set: std::collections::HashSet<&[u8]> =
                candidate_pool.iter().map(Vec::as_slice).collect();
            let eligible = |c: &[u8]| pool_set.contains(c);

            match s.conversation.steward_list.propose_election(
                epoch,
                &candidate_pool,
                self_member_id,
                eligible,
                recovery,
            )? {
                ElectionDecision::Skip(reason) => {
                    if matches!(reason, "no eligible candidates after filter") {
                        info!(
                            conversation = %s.conversation_id,
                            "skipping election: {reason}"
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
                    s.conversation_id.clone(),
                ),
            }
        };

        let stewards_len = proposed_stewards.len();
        let request = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards,
                    election_epoch,
                    retry_round,
                },
            )),
        };

        info!(
            conversation = %conversation_id,
            epoch = election_epoch,
            retry_round,
            stewards = stewards_len,
            recovery,
            "initiating steward election"
        );

        // Elections are conversation-wide decisions — broadcast unbundled
        // so the responsible proposer still votes via the banner.
        Self::initiate_proposal(arc, request, CreatorVote::Deferred)?;

        Ok(())
    }

    /// Layer 3 escalation: file a `Deadlock` ECP after re-election retries
    /// exhaust. Only the deterministic responsible proposer submits;
    /// others no-op. On YES the ECP opens `recovery_mode`.
    pub fn initiate_deadlock_ecp(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let (is_authorized, self_id, epoch, conversation_id) = {
            let s = arc.read_or_err("session")?;
            let mls = s.conversation.expect_mls()?;
            let mls_members = mls.members()?;
            let epoch = mls.current_epoch()?;
            let self_id: &[u8] = &s.self_member_id;
            // Deadlock proposer = election proposer with the stricter
            // predicate (MLS-present and not queued for removal).
            let mls_set: std::collections::HashSet<&[u8]> =
                mls_members.iter().map(Vec::as_slice).collect();
            let conversation_ref = &s.conversation.conversation;
            let authorized = s
                .conversation
                .steward_list
                .election_proposer(|c: &[u8]| {
                    mls_set.contains(c) && !conversation_ref.has_approved_removal(c)
                })
                .is_some_and(|proposer| proposer == self_id);
            (
                authorized,
                Arc::clone(&s.self_member_id),
                epoch,
                s.conversation_id.clone(),
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
        Self::initiate_proposal(arc, request, CreatorVote::Yes)?;
        Ok(())
    }
}
