//! Post-epoch-advance steward housekeeping on `SessionRunner`: list
//! generation/election, pending-update drain, scoring sync, conversation-sync
//! broadcast.
//!
//! Methods that file new proposals (`try_initiate_steward_election`,
//! `try_initiate_deadlock_ecp`, `process_buffered_updates`,
//! `check_and_initiate_score_removals`) are associated functions taking
//! `Arc<RwLock<SessionRunner>>` so they can call
//! [`SessionRunner::initiate_proposal`] which itself spawns a background
//! task. The rest are `&self` / `&mut self` methods on the runner.

use std::sync::{Arc, RwLock};

use tracing::{error, info};

use super::lock::LockExt;
use super::runner::send_packet;

use crate::{
    app::{CreatorVote, SessionRunner, UserError},
    core::{
        ConsensusPlugin, ConversationPluginsFactory, ElectionDecision, PeerScoringPlugin,
        StewardListPlugin, member_set, scoring_member_diff, target_identity_of,
    },
    identity::ShortId,
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationSync, ConversationUpdateRequest, PeerScore,
        StewardElectionProposal, TimingConfig, ViolationEvidence, conversation_update_request,
    },
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    // ── Public API ───────────────────────────────────────────────────

    /// Add any MLS members not yet tracked in scoring, and drop scored
    /// entries for identities no longer in MLS. Diffing is delegated to
    /// [`scoring_member_diff`]; this method only applies the diff.
    pub fn sync_scoring_members(&mut self, mls_members: &[Vec<u8>]) {
        let scored: Vec<Vec<u8>> = self
            .handle
            .scoring
            .all_members_with_scores()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let diff = scoring_member_diff(&scored, mls_members);
        for member_id in &diff.to_add {
            // Under the standard config (`default > threshold`) this
            // returns no events; an exotic config could surface a fresh
            // member as below-threshold, but the score-removal chain
            // doesn't fire on membership-sync ticks today.
            let _events = self.handle.scoring.add_member(member_id);
        }
        for member_id in &diff.to_remove {
            self.handle.scoring.remove_member(member_id);
        }
    }

    /// Post-epoch-advance sequence: (1) auto-fill if membership dropped
    /// below `sn_min`, (2) kick off an election if the list is exhausted.
    /// Election-initiate failures are logged, not surfaced — conversation
    /// state may legitimately reject a new proposal right now.
    pub fn steward_list_housekeeping(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        arc.write_or_err("session")?.try_auto_fill_steward_list()?;
        if let Err(e) = Self::try_initiate_steward_election(arc, false, None) {
            let conv_name = arc.read_or_err("session")?.conversation_name.clone();
            info!(conversation = %conv_name, error = %e, "election initiation deferred");
        }
        Ok(())
    }

    /// Regenerate the steward list at the current epoch against the current
    /// MLS member set — same effect as a successful election. Intended for
    /// tests and administrative tooling.
    pub fn regenerate_steward_list(&mut self) -> Result<(), UserError> {
        let current_epoch = self.handle.expect_mls()?.current_epoch()?;
        let members = self.handle.conversation_members()?;
        let sn = self
            .handle
            .steward_list
            .config()
            .compute_list_size(members.len());
        // Test/admin regenerate — no election, no retry seed.
        let _events = self
            .handle
            .steward_list
            .install_list(current_epoch, &members, sn, 0)?;
        Ok(())
    }

    /// Drop Add entries whose target is now a member and Remove entries
    /// whose target is now gone, then expire entries older than
    /// `pending_update_max_epochs`.
    pub fn prune_pending_updates_after_commit(&mut self) -> Result<(), UserError> {
        let (current_epoch, members, max_age) = {
            let Some(mls) = self.handle.mls() else {
                return Ok(());
            };
            (
                mls.current_epoch()?,
                self.handle.conversation_members()?,
                self.handle.config.pending_update_max_epochs,
            )
        };

        let before = self.handle.conversation.pending_update_count();
        self.handle
            .conversation
            .prune_pending_updates_for_members(&members);
        let expired = self
            .handle
            .conversation
            .expire_pending_updates(current_epoch, max_age);
        let after = self.handle.conversation.pending_update_count();
        if before != after {
            info!(
                conversation = %self.conversation_name,
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
        let (current_epoch, to_propose, conversation_name): (
            u64,
            Vec<ConversationUpdateRequest>,
            String,
        ) = {
            let s = arc.read_or_err("session")?;

            let (current_epoch, members) = match s.handle.mls() {
                Some(mls) => (mls.current_epoch()?, s.handle.conversation_members()?),
                None => (0, Vec::new()),
            };
            let self_identity: &[u8] = &s.self_identity;
            let eligible = s.handle.conversation.steward_eligibility(&members);
            let is_live = s
                .handle
                .steward_list
                .epoch_steward(current_epoch, &eligible)
                .is_some_and(|es| es == self_identity);
            if !is_live {
                return Ok(());
            }

            // Collect buffered updates whose target isn't already in an
            // active proposal queue. Both the voting and approved queues
            // live on `Conversation`.
            let approved = s.handle.conversation.approved_proposals();
            let approved_targets: std::collections::HashSet<&[u8]> =
                approved.values().filter_map(target_identity_of).collect();
            let members_set = member_set(&members);

            let to_propose: Vec<ConversationUpdateRequest> = s
                .handle
                .conversation
                .pending_updates()
                .iter()
                .filter(|(id, _)| !approved_targets.contains(id.as_slice()))
                .filter(|(id, p)| {
                    // Drop Add for already-member and Remove for non-member.
                    let is_member = members_set.contains(id.as_slice());
                    match p.request.payload.as_ref() {
                        Some(conversation_update_request::Payload::InviteMember(_)) => !is_member,
                        Some(conversation_update_request::Payload::RemoveMember(_)) => is_member,
                        _ => false,
                    }
                })
                .map(|(_, p)| p.request.clone())
                .collect();
            (current_epoch, to_propose, s.conversation_name.clone())
        };

        if to_propose.is_empty() {
            return Ok(());
        }

        info!(
            conversation = %conversation_name,
            epoch = current_epoch,
            count = to_propose.len(),
            "promoting buffered updates to proposals"
        );

        // Buffered updates inherit the same banner path as fresh
        // steward-auto-propose — the steward still decides per proposal.
        for request in to_propose {
            if let Err(e) = Self::initiate_proposal(arc, request, CreatorVote::Deferred) {
                info!(
                    conversation = %conversation_name,
                    error = %e,
                    "buffered proposal deferred"
                );
            }
        }
        Ok(())
    }

    /// Build the encrypted `ConversationSync` packet without sending. Used
    /// by callers that need to release the runner lock before awaiting on
    /// the transport. Returns `Ok(None)` when there's no steward list yet.
    pub(crate) fn build_conversation_sync_packet(
        &self,
    ) -> Result<Option<crate::ds::OutboundPacket>, UserError> {
        // Sparse snapshot — only members whose score has diverged
        // from `default_score`. Joiners init every member at default
        // via membership sync before applying the snapshot, so
        // missing entries imply default. Saves wire size at scale
        // (Waku message budget concern past ~1k members).
        let scores: Vec<PeerScore> = self
            .handle
            .scoring
            .snapshot()
            .diverged
            .into_iter()
            .map(|(id, score)| PeerScore {
                member_id: id,
                score,
            })
            .collect();

        let list = match self.handle.steward_list.current_list() {
            Some(l) => l,
            None => return Ok(None),
        };

        let timing = TimingConfig::from(&self.handle.config);

        // Filter ghosts and queued-removal targets so joiners don't
        // inherit stewards they would have to walk past on the very
        // first epoch.
        let mls = self.handle.expect_mls()?;
        let mls_members = self.handle.conversation_members()?;
        let eligible = self.handle.conversation.steward_eligibility(&mls_members);
        let steward_members = self.handle.steward_list.steward_members(&eligible);

        // `retry_round` is the seed that produced the *stored* list —
        // a frozen tag on `StewardList`, not the plug-in's dynamic
        // counter for the next attempt (which resets to 0 on accept).
        // Joiners re-derive the ordering from this seed.
        let sync = ConversationSync {
            steward_members,
            election_epoch: list.election_epoch(),
            sn_min: list.config().sn_min as u32,
            sn_max: list.config().sn_max as u32,
            allow_subset_candidates: self.handle.steward_list.config().allow_subset_candidates,
            peer_scores: scores,
            timing: Some(timing),
            retry_round: list.retry_round(),
            max_reelection_attempts: self.handle.steward_list.max_retries(),
            liveness_criteria_yes: self.handle.config.liveness_criteria_yes,
            threshold_peer_score: self.handle.scoring.threshold(),
            pending_update_max_epochs: self.handle.config.pending_update_max_epochs,
        };

        let app_msg: AppMessage = sync.into();
        Ok(Some(mls.build_message(&app_msg, &self.app_id)?))
    }

    /// Broadcast steward list + protocol config + peer scores + timing as
    /// an encrypted `ConversationSync`. Steward calls this after an Add-bearing
    /// commit so new joiners can fully participate. Idempotent for members
    /// who already have a steward list.
    ///
    /// Takes `&Arc<RwLock<Self>>` so the runner lock is released before
    /// awaiting on the transport.
    pub async fn send_conversation_sync(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let (transport, packet, conversation_name) = {
            let s = arc.read_or_err("session")?;
            let Some(packet) = s.build_conversation_sync_packet()? else {
                return Ok(());
            };
            (
                Arc::clone(s.transport()),
                packet,
                s.conversation_name.clone(),
            )
        };
        send_packet(&transport, packet).await?;
        info!(conversation = %conversation_name, "conversation sync sent");
        Ok(())
    }

    /// Steward-only: file `ScoreBelowThreshold` ECPs for any member whose
    /// score fell at or below the removal threshold. Skips self and any
    /// target already covered by a pending removal.
    pub fn check_and_initiate_score_removals(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        // Reactive entry: callers chain into this after a scoring apply
        // emitted a downward cross, so we expect at least one tracked
        // member to be at-or-below threshold. The scan is the source of
        // truth — events just trigger the look.
        let (epoch, to_remove, self_id_arc, conversation_name) = {
            let mut s = arc.write_or_err("session")?;
            let epoch = s.handle.expect_mls()?.current_epoch()?;
            let self_id_arc = Arc::clone(&s.self_identity);
            let is_steward = s.handle.steward_list.is_steward(&self_id_arc);
            if !is_steward {
                return Ok(());
            }
            // Two dedup gates:
            //   - `has_pending_removal` — there's an in-flight ECP for
            //     this target (live consensus session).
            //   - `is_pending_removal` — `RemoveMember(target)` is
            //     already queued in `approved_proposals` waiting for
            //     the next commit. Without this gate, a just-resolved
            //     SCORE_BELOW_THRESHOLD ECP (which clears
            //     `pending_removal_targets` on resolve, but leaves the
            //     target in `approved_proposals` with their score still
            //     ≤ threshold) would re-fire a duplicate ECP for the
            //     same target before the RemoveMember commits.
            let self_id: &[u8] = &self_id_arc;
            let to_remove: Vec<(Vec<u8>, i64)> = s
                .handle
                .scoring
                .members_below_threshold()
                .into_iter()
                .filter(|id| id.as_slice() != self_id)
                .filter(|id| !s.handle.conversation.has_pending_removal(id))
                .filter(|id| !s.handle.conversation.is_pending_removal(id))
                .filter_map(|id| {
                    let score = s.handle.scoring.score_for(&id)?;
                    Some((id, score))
                })
                .collect();
            for (id, _) in &to_remove {
                s.handle.conversation.observe_pending_removal(id.clone());
            }
            (epoch, to_remove, self_id_arc, s.conversation_name.clone())
        };

        // Submit proposals without holding the lock.
        for (target_id, current_score) in to_remove {
            let evidence =
                ViolationEvidence::score_below_threshold(target_id.clone(), epoch, current_score)
                    .with_creator(self_id_arc.to_vec());
            let request = evidence.into_update_request()?;

            info!(
                conversation = %conversation_name,
                target = %ShortId::new(&target_id),
                score = current_score,
                "initiating SCORE_BELOW_THRESHOLD removal"
            );
            // SCORE_BELOW_THRESHOLD is self-executing: threshold crossed ⇒
            // member must be removed. The steward's vote is YES by
            // protocol, so we bundle it at submit and skip the banner.
            if let Err(e) = Self::initiate_proposal(arc, request, CreatorVote::Yes) {
                arc.write_or_err("session")?
                    .handle
                    .conversation
                    .resolve_pending_removal(&target_id);
                error!(
                    conversation = %conversation_name,
                    target = %ShortId::new(&target_id),
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
    /// queued-removal targets out of the candidate pool. `extra_exclude`
    /// drops one more identity not yet in `approved_proposals`.
    pub(crate) fn try_initiate_steward_election(
        arc: &Arc<RwLock<Self>>,
        recovery: bool,
        extra_exclude: Option<&[u8]>,
    ) -> Result<(), UserError> {
        let (proposed_stewards, election_epoch, retry_round, conversation_name) = {
            let s = arc.read_or_err("session")?;
            let epoch = s.handle.expect_mls()?.current_epoch()?;
            let mls_members = s.handle.conversation_members()?;
            let self_identity: &[u8] = &s.self_identity;

            // `has_election_in_flight` is a proposal-queue check, not a
            // steward-list one — gated here, before the plug-in call.
            if s.handle.conversation.has_election_in_flight() {
                return Ok(());
            }

            // Build the candidate pool: MLS members minus pending
            // removals (recovery only) minus any explicit exclude.
            let candidate_pool: Vec<Vec<u8>> = mls_members
                .iter()
                .filter(|m| {
                    if extra_exclude.is_some_and(|x| x == m.as_slice()) {
                        return false;
                    }
                    if recovery && s.handle.conversation.is_pending_removal(m) {
                        return false;
                    }
                    true
                })
                .cloned()
                .collect();
            let pool_set: std::collections::HashSet<&[u8]> =
                candidate_pool.iter().map(Vec::as_slice).collect();
            let eligible = |c: &[u8]| pool_set.contains(c);

            match s.handle.steward_list.propose_election(
                epoch,
                &candidate_pool,
                self_identity,
                &eligible,
                recovery,
            )? {
                ElectionDecision::Skip(reason) => {
                    if matches!(reason, "no eligible candidates after filter") {
                        info!(
                            conversation = %s.conversation_name,
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
                    s.conversation_name.clone(),
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
            conversation = %conversation_name,
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
    pub(crate) fn try_initiate_deadlock_ecp(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let (is_authorized, self_id, epoch, conversation_name) = {
            let s = arc.read_or_err("session")?;
            let mls_members = s.handle.conversation_members()?;
            let self_id: &[u8] = &s.self_identity;
            // Deadlock proposer = election proposer with the stricter
            // predicate (MLS-present and not queued for removal).
            let mls_set: std::collections::HashSet<&[u8]> =
                mls_members.iter().map(Vec::as_slice).collect();
            let conversation_ref = &s.handle.conversation;
            let authorized = s
                .handle
                .steward_list
                .election_proposer(&|c: &[u8]| {
                    mls_set.contains(c) && !conversation_ref.is_pending_removal(c)
                })
                .is_some_and(|proposer| proposer == self_id);
            let epoch = s.handle.expect_mls()?.current_epoch()?;
            (
                authorized,
                Arc::clone(&s.self_identity),
                epoch,
                s.conversation_name.clone(),
            )
        };

        if !is_authorized {
            return Ok(());
        }

        let request = ViolationEvidence::deadlock(epoch)
            .with_creator(self_id.to_vec())
            .into_update_request()?;

        info!(
            conversation = %conversation_name,
            epoch, "initiating Deadlock ECP"
        );

        // Bundle YES — the proposer's observation that the deadlock is
        // real is their vote.
        Self::initiate_proposal(arc, request, CreatorVote::Yes)?;
        Ok(())
    }

    // ── Private ──────────────────────────────────────────────────────

    /// Plug-in decides whether the current list still satisfies its
    /// policy (the deterministic impl re-installs when membership drops
    /// below `sn_min`). Coordinator just supplies `epoch` + `members`
    /// and drains events.
    fn try_auto_fill_steward_list(&mut self) -> Result<(), UserError> {
        let epoch = self.handle.expect_mls()?.current_epoch()?;
        let members = self.handle.conversation_members()?;
        let _events = self.handle.steward_list.maybe_auto_fill(epoch, &members)?;
        Ok(())
    }
}
