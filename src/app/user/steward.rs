//! Post-epoch-advance steward housekeeping: list generation/election,
//! pending-update drain, scoring sync, group-sync broadcast.

use tracing::{error, info};

use crate::{
    app::{StateChangeHandler, User, UserError},
    core::{
        DeMlsProvider, ElectionDecision, GroupEventHandler, PeerScoringPlugin, StewardListPlugin,
        member_set, scoring_member_diff, target_identity_of,
    },
    identity::{Identity, ShortId},
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        AppMessage, GroupSync, GroupUpdateRequest, PeerScore, StewardElectionProposal,
        TimingConfig, ViolationEvidence, group_update_request,
    },
};

impl<
    P: DeMlsProvider,
    M: MlsService,
    Sc: PeerScoringPlugin,
    St: StewardListPlugin,
    I: Identity,
    H: GroupEventHandler + 'static,
    SCH: StateChangeHandler + 'static,
> User<P, M, Sc, St, I, H, SCH>
{
    /// Add any MLS members not yet tracked in scoring, and drop scored
    /// entries for identities no longer in MLS. Diffing is delegated to
    /// [`scoring_member_diff`]; this method only applies the diff.
    pub async fn sync_scoring_members(&self, group_name: &str, mls_members: &[Vec<u8>]) {
        let Some(entry_arc) = self.lookup_entry(group_name).await else {
            return;
        };
        let mut entry = entry_arc.write().await;
        let scored: Vec<Vec<u8>> = entry
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
            let _events = entry.scoring.add_member(member_id);
        }
        for member_id in &diff.to_remove {
            entry.scoring.remove_member(member_id);
        }
    }

    /// Plug-in decides whether the current list still satisfies its
    /// policy (the deterministic impl re-installs when membership drops
    /// below `sn_min`). Coordinator just supplies `epoch` + `members`
    /// and drains events.
    async fn try_auto_fill_steward_list(&self, group_name: &str) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let mut entry = entry_arc.write().await;
        let mls = entry.expect_mls()?;
        let epoch = mls.current_epoch()?;
        let members = entry.group_members()?;
        let _events = entry.steward.maybe_auto_fill(epoch, &members)?;
        Ok(())
    }

    /// Submit a steward-election proposal. Only the deterministic responsible
    /// proposer actually submits; others no-op, so this is safe to call from
    /// every poll tick without double-proposing.
    ///
    /// `recovery = true` bypasses the list-exhaustion gate and filters
    /// queued-removal targets out of the candidate pool. `extra_exclude`
    /// drops one more identity not yet in `approved_proposals`.
    pub(super) async fn try_initiate_steward_election(
        &self,
        group_name: &str,
        recovery: bool,
        extra_exclude: Option<&[u8]>,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let (proposed_stewards, election_epoch, retry_round) = {
            let entry = entry_arc.read().await;
            let mls = entry.expect_mls()?;
            let epoch = mls.current_epoch()?;
            let mls_members = entry.group_members()?;
            let self_identity = self.identity().identity_bytes();

            // `has_election_in_flight` is a proposal-queue check, not a
            // steward-list one — gated here, before the plug-in call.
            if entry.group.has_election_in_flight() {
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
                    if recovery && entry.group.is_pending_removal(m) {
                        return false;
                    }
                    true
                })
                .cloned()
                .collect();
            let pool_set: std::collections::HashSet<&[u8]> =
                candidate_pool.iter().map(Vec::as_slice).collect();
            let eligible = |c: &[u8]| pool_set.contains(c);

            match entry.steward.propose_election(
                epoch,
                &candidate_pool,
                self_identity,
                &eligible,
                recovery,
            )? {
                ElectionDecision::Skip(reason) => {
                    if matches!(reason, "no eligible candidates after filter") {
                        info!(group = group_name, "skipping election: {reason}");
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

        let stewards_len = proposed_stewards.len();
        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards,
                    election_epoch,
                    retry_round,
                },
            )),
        };

        info!(
            group = group_name,
            epoch = election_epoch,
            retry_round,
            stewards = stewards_len,
            recovery,
            "initiating steward election"
        );

        // Elections are group decisions — broadcast unbundled so the
        // responsible proposer still votes via the banner.
        self.initiate_proposal(group_name.to_string(), request, None)
            .await?;

        Ok(())
    }

    /// Layer 3 escalation: file a `Deadlock` ECP after re-election retries
    /// exhaust. Only the deterministic responsible proposer submits;
    /// others no-op. On YES the ECP opens `recovery_mode`.
    pub(super) async fn try_initiate_deadlock_ecp(
        &self,
        group_name: &str,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let (is_authorized, self_id, epoch) = {
            let entry = entry_arc.read().await;
            let mls = entry.expect_mls()?;
            let mls_members = entry.group_members()?;
            let self_id = self.identity().identity_bytes();
            // Deadlock proposer = election proposer with the stricter
            // predicate (MLS-present and not queued for removal).
            let mls_set: std::collections::HashSet<&[u8]> =
                mls_members.iter().map(Vec::as_slice).collect();
            let group_ref = &entry.group;
            let authorized = entry
                .steward
                .election_proposer(&|c: &[u8]| {
                    mls_set.contains(c) && !group_ref.is_pending_removal(c)
                })
                .is_some_and(|proposer| proposer == self_id);
            let epoch = mls.current_epoch()?;
            (authorized, self_id, epoch)
        };

        if !is_authorized {
            return Ok(());
        }

        let request = ViolationEvidence::deadlock(epoch)
            .with_creator(self_id.to_vec())
            .into_update_request()?;

        info!(group = group_name, epoch, "initiating Deadlock ECP");

        // Bundle YES — the proposer's observation that the deadlock is
        // real is their vote.
        self.initiate_proposal(group_name.to_string(), request, Some(true))
            .await?;
        Ok(())
    }

    /// Post-epoch-advance sequence: (1) auto-fill if membership dropped
    /// below `sn_min`, (2) kick off an election if the list is exhausted.
    /// Election-initiate failures are logged, not surfaced — group state
    /// may legitimately reject a new proposal right now.
    pub async fn steward_list_housekeeping(&self, group_name: &str) -> Result<(), UserError> {
        self.try_auto_fill_steward_list(group_name).await?;
        if let Err(e) = self
            .try_initiate_steward_election(group_name, false, None)
            .await
        {
            info!(group = group_name, error = %e, "election initiation deferred");
        }
        Ok(())
    }

    /// Regenerate the steward list at the current epoch against the current
    /// MLS member set — same effect as a successful election. Intended for
    /// tests and administrative tooling.
    pub async fn regenerate_steward_list(&self, group_name: &str) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let (current_epoch, members) = {
            let entry = entry_arc.read().await;
            let mls = entry.expect_mls()?;
            let current_epoch = mls.current_epoch()?;
            let members = entry.group_members()?;
            (current_epoch, members)
        };

        let mut entry = entry_arc.write().await;
        let sn = entry.steward.config().compute_list_size(members.len());
        // Test/admin regenerate — no election, no retry seed.
        let _events = entry.steward.install_list(current_epoch, &members, sn, 0)?;
        Ok(())
    }

    /// Drop Add entries whose target is now a member and Remove entries
    /// whose target is now gone, then expire entries older than
    /// `pending_update_max_epochs`.
    pub async fn prune_pending_updates_after_commit(
        &self,
        group_name: &str,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let (current_epoch, members, max_age) = {
            let entry = entry_arc.read().await;
            let Some(mls) = entry.mls() else {
                return Ok(());
            };
            (
                mls.current_epoch()?,
                entry.group_members()?,
                entry.config.pending_update_max_epochs,
            )
        };

        let mut entry = entry_arc.write().await;
        let before = entry.group.pending_update_count();
        entry.group.prune_pending_updates_for_members(&members);
        let expired = entry.group.expire_pending_updates(current_epoch, max_age);
        let after = entry.group.pending_update_count();
        if before != after {
            info!(
                group = group_name,
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
    pub async fn process_buffered_updates(&self, group_name: &str) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let (current_epoch, to_propose): (u64, Vec<GroupUpdateRequest>) = {
            let entry = entry_arc.read().await;

            let (current_epoch, members) = match entry.mls() {
                Some(mls) => (mls.current_epoch()?, entry.group_members()?),
                None => (0, Vec::new()),
            };
            let self_identity = self.identity().identity_bytes();
            let eligible = entry.group.steward_eligibility(&members);
            let is_live = entry
                .steward
                .epoch_steward(current_epoch, &eligible)
                .is_some_and(|es| es == self_identity);
            if !is_live {
                return Ok(());
            }

            // Collect buffered updates whose target isn't already in the
            // active proposal queues. The approved queue is also in the group.
            let approved = entry.group.approved_proposals();
            let approved_targets: std::collections::HashSet<&[u8]> =
                approved.values().filter_map(target_identity_of).collect();
            let members_set = member_set(&members);

            let to_propose: Vec<GroupUpdateRequest> = entry
                .group
                .pending_updates()
                .iter()
                .filter(|(id, _)| !approved_targets.contains(id.as_slice()))
                .filter(|(id, p)| {
                    // Drop Add for already-member and Remove for non-member.
                    let is_member = members_set.contains(id.as_slice());
                    match p.request.payload.as_ref() {
                        Some(group_update_request::Payload::InviteMember(_)) => !is_member,
                        Some(group_update_request::Payload::RemoveMember(_)) => is_member,
                        _ => false,
                    }
                })
                .map(|(_, p)| p.request.clone())
                .collect();
            (current_epoch, to_propose)
        };

        if to_propose.is_empty() {
            return Ok(());
        }

        info!(
            group = group_name,
            epoch = current_epoch,
            count = to_propose.len(),
            "promoting buffered updates to proposals"
        );

        // Buffered updates inherit the same banner path as fresh
        // steward-auto-propose — the steward still decides per proposal.
        for request in to_propose {
            if let Err(e) = self
                .initiate_proposal(group_name.to_string(), request, None)
                .await
            {
                info!(group = group_name, error = %e, "buffered proposal deferred");
            }
        }
        Ok(())
    }

    /// Broadcast steward list + protocol config + peer scores + timing as
    /// an encrypted `GroupSync`. Steward calls this after an Add-bearing
    /// commit so new joiners can fully participate. Idempotent for members
    /// who already have a steward list.
    pub async fn send_group_sync(&self, group_name: &str) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let packet = {
            let entry = entry_arc.read().await;
            // Sparse snapshot — only members whose score has diverged
            // from `default_score`. Joiners init every member at default
            // via membership sync before applying the snapshot, so
            // missing entries imply default. Saves wire size at scale
            // (Waku message budget concern past ~1k members).
            let scores: Vec<PeerScore> = entry
                .scoring
                .snapshot()
                .diverged
                .into_iter()
                .map(|(id, score)| PeerScore {
                    member_id: id,
                    score,
                })
                .collect();

            let list = match entry.steward.current_list() {
                Some(l) => l,
                None => return Ok(()),
            };

            let timing = TimingConfig {
                commit_inactivity_duration_ms: entry
                    .phase_timer
                    .commit_inactivity_duration()
                    .as_millis() as u64,
                freeze_duration_ms: entry.phase_timer.freeze_duration().as_millis() as u64,
                proposal_expiration_ms: entry.config.proposal_expiration.as_millis() as u64,
                consensus_timeout_ms: entry.config.consensus_timeout.as_millis() as u64,
                recovery_inactivity_duration_ms: entry
                    .phase_timer
                    .recovery_inactivity_duration()
                    .as_millis() as u64,
            };

            // Filter ghosts and queued-removal targets so joiners don't
            // inherit stewards they would have to walk past on the very
            // first epoch.
            let mls = entry.expect_mls()?;
            let mls_members = entry.group_members()?;
            let eligible = entry.group.steward_eligibility(&mls_members);
            let steward_members = entry.steward.steward_members(&eligible);

            // `retry_round` is the seed that produced the *stored* list —
            // a frozen tag on `StewardList`, not the plug-in's dynamic
            // counter for the next attempt (which resets to 0 on accept).
            // Joiners re-derive the ordering from this seed.
            let sync = GroupSync {
                steward_members,
                election_epoch: list.election_epoch(),
                sn_min: list.config().sn_min as u32,
                sn_max: list.config().sn_max as u32,
                allow_subset_candidates: entry.steward.config().allow_subset_candidates,
                peer_scores: scores,
                timing: Some(timing),
                retry_round: list.retry_round(),
                max_reelection_attempts: entry.steward.max_retries(),
                liveness_criteria_yes: entry.config.liveness_criteria_yes,
                threshold_peer_score: entry.scoring.threshold(),
                pending_update_max_epochs: entry.config.pending_update_max_epochs,
            };

            let app_msg: AppMessage = sync.into();
            mls.build_message(&app_msg, &self.app_id)?
        };

        self.handler.on_outbound(group_name, packet).await?;
        info!(group = group_name, "group sync sent");
        Ok(())
    }

    /// Steward-only: file `ScoreBelowThreshold` ECPs for any member whose
    /// score fell at or below the removal threshold. Skips self and any
    /// target already covered by a pending removal.
    pub async fn check_and_initiate_score_removals(
        &self,
        group_name: &str,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let self_id = self.identity().identity_bytes();

        // Reactive entry: callers chain into this after a scoring apply
        // emitted a downward cross, so we expect at least one tracked
        // member to be at-or-below threshold. The scan is the source of
        // truth — events just trigger the look.
        let (is_steward, epoch, to_remove): (bool, u64, Vec<(Vec<u8>, i64)>) = {
            let mut entry = entry_arc.write().await;
            let mls = entry.expect_mls()?;
            let is_steward = entry.steward.is_steward(self_id);
            let epoch = mls.current_epoch()?;
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
            let to_remove = entry
                .scoring
                .members_below_threshold()
                .into_iter()
                .filter(|id| *id != self_id)
                .filter(|id| !entry.group.has_pending_removal(id))
                .filter(|id| !entry.group.is_pending_removal(id))
                .filter_map(|id| {
                    let score = entry.scoring.score_for(&id)?;
                    Some((id, score))
                })
                .collect::<Vec<_>>();
            for (id, _) in &to_remove {
                entry.group.observe_pending_removal(id.clone());
            }
            (is_steward, epoch, to_remove)
        };

        if !is_steward {
            return Ok(());
        }

        // Submit proposals without holding the lock.
        for (target_id, current_score) in to_remove {
            let evidence =
                ViolationEvidence::score_below_threshold(target_id.clone(), epoch, current_score)
                    .with_creator(self_id.to_vec());
            let request = evidence.into_update_request()?;

            info!(
                group = group_name,
                target = %ShortId(&target_id),
                score = current_score,
                "initiating SCORE_BELOW_THRESHOLD removal"
            );
            // SCORE_BELOW_THRESHOLD is self-executing: threshold crossed ⇒
            // member must be removed. The steward's vote is YES by
            // protocol, so we bundle it at submit and skip the banner.
            if let Err(e) = self
                .initiate_proposal(group_name.to_string(), request, Some(true))
                .await
            {
                if let Some(entry_arc) = self.lookup_entry(group_name).await {
                    let mut entry = entry_arc.write().await;
                    entry.group.resolve_pending_removal(&target_id);
                }
                error!(
                    group = group_name,
                    target = %ShortId(&target_id),
                    error = %e,
                    "SCORE_BELOW_THRESHOLD vote failed to start"
                );
            }
        }

        Ok(())
    }
}
