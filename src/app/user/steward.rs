//! Post-epoch-advance steward housekeeping: list generation/election,
//! pending-update drain, scoring sync, group-sync broadcast.

use tracing::{error, info};

use crate::{
    app::{StateChangeHandler, User, UserError},
    core::{
        DeMlsProvider, Group, GroupEventHandler, StewardList, build_message, group_members,
        target_identity_of,
    },
    mls_crypto::ShortId,
    protos::de_mls::messages::v1::{
        AppMessage, GroupSync, GroupUpdateRequest, PeerScore, StewardElectionProposal,
        TimingConfig, ViolationEvidence, group_update_request,
    },
};

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Add any MLS members not yet tracked in scoring, and drop scored
    /// entries for identities no longer in MLS.
    pub fn sync_scoring_members(&self, group_name: &str, group: &Group) {
        let mls_members = match group_members(group, &self.mls_service) {
            Ok(m) => m,
            Err(_) => return,
        };

        let mut scoring = self.scoring();
        let scored = scoring.all_members_with_scores(group_name);
        let scored_ids: std::collections::HashSet<Vec<u8>> =
            scored.iter().map(|(id, _)| id.clone()).collect();
        let mls_ids: std::collections::HashSet<Vec<u8>> = mls_members.iter().cloned().collect();

        for member_id in &mls_ids {
            if !scored_ids.contains(member_id) {
                scoring.add_member(group_name, member_id);
            }
        }
        for member_id in &scored_ids {
            if !mls_ids.contains(member_id) {
                scoring.remove_member(group_name, member_id);
            }
        }
    }

    /// RFC rule: when `member_count < sn_min`, every member is a steward.
    /// Re-derive the list to cover that case after a membership-changing commit.
    async fn try_auto_fill_steward_list(&self, group_name: &str) -> Result<(), UserError> {
        let (needs_fill, members) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let members = group_members(&entry.group, &self.mls_service)?;
            let needs = members.len() < entry.group.protocol_config().sn_min;
            (needs, members)
        };

        if !needs_fill {
            return Ok(());
        }

        let epoch = self.mls_service.current_epoch(group_name)?;
        {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            let sn = entry
                .group
                .protocol_config()
                .compute_list_size(members.len());
            // sn_min auto-fill isn't an election outcome — no retry seed.
            entry
                .group
                .generate_and_set_steward_list(epoch, &members, sn, 0)?;
        }

        Ok(())
    }

    /// Submit a steward-election proposal. Only the deterministic responsible
    /// proposer (first live member of the previous list) actually submits;
    /// others no-op, so this is safe to call from every poll tick without
    /// double-proposing. `retry_round` comes from `Group::reelection_round`
    /// (bumps on reject).
    ///
    /// `recovery` switches the call between the two trigger modes:
    /// - `false` (housekeeping): only fires when the steward list is exhausted
    ///   at the current epoch. Candidate pool is the full MLS member set.
    /// - `true` (Layer 2 recovery): bypasses the exhaustion gate so we can
    ///   regenerate the list when the live walk has yielded nothing. Members
    ///   with a queued `RemoveMember` are filtered out of the candidate pool
    ///   so the new list doesn't re-include them.
    ///
    /// `extra_exclude` adds one more identity to drop from the candidate pool
    /// — used by proactive callers (banner / leaver) whose target isn't in
    /// `approved_proposals` yet at submission time.
    pub(super) async fn try_initiate_steward_election(
        &self,
        group_name: &str,
        recovery: bool,
        extra_exclude: Option<&[u8]>,
    ) -> Result<(), UserError> {
        let (members, election_epoch, config, retry_round) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let epoch = self.mls_service.current_epoch(group_name)?;
            if !recovery && !entry.group.is_steward_list_exhausted(epoch) {
                return Ok(());
            }
            if entry.group.has_election_in_flight() {
                return Ok(());
            }
            let mls_members = group_members(&entry.group, &self.mls_service)?;

            let self_identity = self.mls_service.wallet_bytes();
            let is_authorized = entry
                .group
                .steward_list()
                .and_then(|list| {
                    list.responsible_election_proposer(|c| mls_members.iter().any(|m| m == c))
                })
                .is_some_and(|proposer| proposer == self_identity);
            if !is_authorized {
                return Ok(());
            }

            let candidate_pool: Vec<Vec<u8>> = mls_members
                .into_iter()
                .filter(|m| {
                    if extra_exclude.is_some_and(|x| x == m.as_slice()) {
                        return false;
                    }
                    if recovery && entry.group.is_pending_removal(m) {
                        return false;
                    }
                    true
                })
                .collect();

            if candidate_pool.is_empty() {
                info!(
                    group = group_name,
                    "skipping election: no eligible candidates after filter"
                );
                return Ok(());
            }

            let config = entry.group.protocol_config().clone();
            let retry_round = entry.group.reelection_round();
            (candidate_pool, epoch, config, retry_round)
        };

        let sn = config.compute_list_size(members.len());
        let proposed_list = StewardList::generate(
            election_epoch,
            group_name.as_bytes(),
            &members,
            sn,
            config,
            retry_round,
        )?;

        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards: proposed_list.members().to_vec(),
                    election_epoch,
                    retry_round,
                },
            )),
        };

        info!(
            group = group_name,
            epoch = election_epoch,
            retry_round,
            stewards = proposed_list.len(),
            recovery,
            "initiating steward election"
        );

        // Elections are group decisions — broadcast unbundled so the
        // responsible proposer still votes via the banner.
        self.initiate_proposal(group_name.to_string(), request, None)
            .await?;

        Ok(())
    }

    /// Layer 3 escalation: file a `Deadlock` ECP after Layer 2's
    /// re-election retries are exhausted. Only the deterministic
    /// responsible proposer (first eligible member of the previous
    /// steward list) actually submits — others no-op, so it's safe to
    /// call from every member's `handle_election_rejected`. On YES the
    /// ECP opens `recovery_mode` (any-member commit) until a fresh
    /// election lands.
    pub(super) async fn try_initiate_deadlock_ecp(
        &self,
        group_name: &str,
    ) -> Result<(), UserError> {
        let (is_authorized, self_id, epoch) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let mls_members = group_members(&entry.group, &self.mls_service)?;
            let self_id = self.mls_service.wallet_bytes();
            let is_authorized = entry
                .group
                .steward_list()
                .and_then(|list| {
                    list.responsible_election_proposer(|c| {
                        mls_members.iter().any(|m| m == c) && !entry.group.is_pending_removal(c)
                    })
                })
                .is_some_and(|proposer| proposer == self_id);
            let epoch = self.mls_service.current_epoch(group_name)?;
            (is_authorized, self_id, epoch)
        };

        if !is_authorized {
            return Ok(());
        }

        let request = ViolationEvidence::deadlock(epoch)
            .with_creator(self_id)
            .into_update_request()?;

        info!(group = group_name, epoch, "initiating Deadlock ECP");

        // Bundle the YES vote at submit — the proposer's observation that
        // the deadlock is real *is* their YES.
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
        let current_epoch = self.mls_service.current_epoch(group_name)?;
        let members = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            group_members(&entry.group, &self.mls_service)?
        };

        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
        let sn = entry
            .group
            .protocol_config()
            .compute_list_size(members.len());
        // Test/admin regenerate — no election, no retry seed.
        entry
            .group
            .generate_and_set_steward_list(current_epoch, &members, sn, 0)?;
        Ok(())
    }

    /// Drop Add entries whose target is now a member and Remove entries
    /// whose target is now gone, then expire entries older than
    /// `pending_update_max_epochs`.
    pub async fn prune_pending_updates_after_commit(
        &self,
        group_name: &str,
    ) -> Result<(), UserError> {
        let current_epoch = self.mls_service.current_epoch(group_name)?;
        let max_age = self.default_group_config.pending_update_max_epochs;

        let members = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            if !self.mls_service.has_group(entry.group.group_name()) {
                return Ok(());
            }
            group_members(&entry.group, &self.mls_service)?
        };

        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
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
        let current_epoch = self.mls_service.current_epoch(group_name)?;

        let to_propose: Vec<GroupUpdateRequest> = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

            let members = if self.mls_service.has_group(entry.group.group_name()) {
                group_members(&entry.group, &self.mls_service)?
            } else {
                Vec::new()
            };
            if !entry.group.is_live_epoch_steward(current_epoch, &members) {
                return Ok(());
            }

            // Collect buffered updates whose target isn't already in the
            // active proposal queues. The approved queue is also in the group.
            let approved = entry.group.approved_proposals();
            let approved_targets: std::collections::HashSet<Vec<u8>> = approved
                .values()
                .filter_map(|req| target_identity_of(req).map(|id| id.to_vec()))
                .collect();
            let members_set: std::collections::HashSet<Vec<u8>> = members.iter().cloned().collect();

            entry
                .group
                .pending_updates()
                .iter()
                .filter(|(id, _)| !approved_targets.contains(*id))
                .filter(|(id, p)| {
                    // Drop Add for already-member and Remove for non-member.
                    let is_member = members_set.contains(*id);
                    match p.request.payload.as_ref() {
                        Some(group_update_request::Payload::InviteMember(_)) => !is_member,
                        Some(group_update_request::Payload::RemoveMember(_)) => is_member,
                        _ => false,
                    }
                })
                .map(|(_, p)| p.request.clone())
                .collect()
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
        // Read scores under the sync Mutex first — no `.await` held across it.
        let scores: Vec<PeerScore> = {
            let scoring = self.scoring();
            scoring
                .all_members_with_scores(group_name)
                .into_iter()
                .map(|(id, score)| PeerScore {
                    member_id: id,
                    score,
                })
                .collect()
        };

        let packet = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

            let list = match entry.group.steward_list() {
                Some(l) => l,
                None => return Ok(()),
            };

            let timing = TimingConfig {
                epoch_duration_ms: entry.state_machine.epoch_duration().as_millis() as u64,
                freeze_duration_ms: entry.state_machine.freeze_duration().as_millis() as u64,
                proposal_expiration_ms: self.default_group_config.proposal_expiration.as_millis()
                    as u64,
                consensus_timeout_ms: self.default_group_config.consensus_timeout.as_millis()
                    as u64,
                retry_inactivity_duration_ms: entry
                    .state_machine
                    .retry_inactivity_duration()
                    .as_millis() as u64,
            };

            // Filter ghosts and queued-removal targets so joiners don't
            // inherit stewards they would have to walk past on the very
            // first epoch.
            let mls_members = group_members(&entry.group, &self.mls_service)?;
            let steward_members = entry.group.live_steward_members(&mls_members);

            // `retry_round` is the seed that produced the *stored* list —
            // a frozen tag on `StewardList`, not `Group::reelection_round`
            // (the dynamic counter for the next attempt, which resets to 0
            // on accept). Joiners re-derive the ordering from this seed.
            let sync = GroupSync {
                steward_members,
                election_epoch: list.election_epoch(),
                sn_min: list.config().sn_min as u32,
                sn_max: list.config().sn_max as u32,
                allow_subset_candidates: entry.group.allow_subset_candidates(),
                peer_scores: scores,
                timing: Some(timing),
                retry_round: list.retry_round(),
                max_reelection_retries: entry.group.max_reelection_retries(),
            };

            let app_msg: AppMessage = sync.into();
            build_message(&entry.group, &self.mls_service, &app_msg, &self.app_id)?
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
        let (is_steward, epoch, self_id) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            (
                entry.group.is_steward(),
                self.mls_service.current_epoch(group_name)?,
                self.mls_service.wallet_bytes(),
            )
        };

        if !is_steward {
            return Ok(());
        }

        let targets: Vec<(Vec<u8>, i64)> = {
            let scoring = self.scoring();
            scoring
                .members_below_threshold(group_name)
                .into_iter()
                .filter(|id| *id != self_id) // self-removal handled separately
                .map(|id| {
                    let score = scoring.score_for(group_name, &id).unwrap_or(0);
                    (id, score)
                })
                .collect()
        };

        // Mark all targets pending under a single write-lock, then submit
        // proposals outside the lock to avoid holding it across `.await`.
        let mut to_remove = Vec::new();
        {
            let mut groups = self.groups.write().await;
            if let Some(entry) = groups.get_mut(group_name) {
                for (target_id, current_score) in targets {
                    if !entry.group.has_pending_removal(&target_id) {
                        entry.group.observe_pending_removal(target_id.clone());
                        to_remove.push((target_id, current_score));
                    }
                }
            }
        }

        // Submit proposals without holding the lock.
        for (target_id, current_score) in to_remove {
            let evidence =
                ViolationEvidence::score_below_threshold(target_id.clone(), epoch, current_score)
                    .with_creator(self_id.clone());
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
                let mut groups = self.groups.write().await;
                if let Some(entry) = groups.get_mut(group_name) {
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
