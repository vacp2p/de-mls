//! Steward list housekeeping and scoring operations.

use super::*;

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Sync the scoring service's member list with the MLS group's actual members.
    ///
    /// Adds any MLS members not yet tracked and removes scored members no longer in MLS.
    pub fn sync_scoring_members(&self, group_name: &str, group: &Group) {
        let mls_members = match core::group_members(group, &self.mls_service) {
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

    /// Try to auto-fill the steward list when member count is below `sn_min`.
    ///
    /// After any commit that changes membership, the group may have fewer members
    /// than `sn_min`. Per RFC, when `member_count < sn_min` all members become
    /// stewards deterministically. This helper checks the condition and regenerates
    /// the steward list if needed.
    async fn try_auto_fill_steward_list(&self, group_name: &str) -> Result<(), UserError> {
        // Phase 1: read lock -- gather member list and check sn_min threshold.
        let (needs_fill, members) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let members = core::group_members(&entry.group, &self.mls_service)?;
            let needs = members.len() < entry.group.protocol_config().sn_min;
            (needs, members)
        };

        if !needs_fill {
            return Ok(());
        }

        // Phase 2: write lock -- generate and set new list.
        let epoch = self.mls_service.current_epoch(group_name)?;
        {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            let sn = entry
                .group
                .protocol_config()
                .compute_list_size(members.len());
            entry
                .group
                .generate_and_set_steward_list(epoch, &members, sn)?;
        }

        Ok(())
    }

    /// Check if the steward list is exhausted and initiate an election if needed.
    ///
    /// Called after epoch advance. If the list is exhausted for the current epoch,
    /// generates a deterministic proposed steward list and submits an election
    /// proposal for consensus.
    async fn try_initiate_steward_election(&self, group_name: &str) -> Result<(), UserError> {
        let (members, election_epoch, config) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let epoch = self.mls_service.current_epoch(group_name)?;
            if !entry.group.is_steward_list_exhausted(epoch) {
                return Ok(());
            }
            let members = core::group_members(&entry.group, &self.mls_service)?;
            let config = entry.group.protocol_config().clone();
            (members, epoch, config)
        };

        // Generate the proposed list deterministically
        let sn = config.compute_list_size(members.len());
        let proposed_list = crate::core::StewardList::generate(
            election_epoch,
            group_name.as_bytes(),
            &members,
            sn,
            config,
        )?;

        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards: proposed_list.members().to_vec(),
                    election_epoch,
                },
            )),
        };

        info!(
            "Steward list exhausted at epoch {election_epoch} for group {group_name}, \
             initiating election with {} stewards",
            proposed_list.len()
        );

        self.initiate_proposal(group_name.to_string(), request)
            .await?;

        Ok(())
    }

    /// Post-epoch-advance steward list housekeeping.
    ///
    /// Runs the post-epoch sequence after any commit that advances the epoch:
    /// 1. Auto-fill steward list if membership dropped below sn_min
    /// 2. Initiate steward election if the list is exhausted
    ///
    /// Steward flag is derived automatically from `steward_list.contains(self_identity)`.
    pub async fn steward_list_housekeeping(&self, group_name: &str) -> Result<(), UserError> {
        self.try_auto_fill_steward_list(group_name).await?;
        // Election initiation may legitimately fail (e.g., group state doesn't
        // allow new proposals yet). Log and continue -- don't block the caller.
        if let Err(e) = self.try_initiate_steward_election(group_name).await {
            info!("[steward_list_housekeeping] Election initiation deferred: {e}");
        }
        Ok(())
    }

    /// Send the current steward list to the group as an encrypted app message.
    ///
    /// Called by the steward after committing an Add that brings new members into
    /// the group. The new joiner's handle has `steward_list: None` until this
    /// sync message is received and validated.
    ///
    /// Existing members who already have a list will ignore it (idempotent).
    pub async fn send_steward_list_sync(&self, group_name: &str) -> Result<(), UserError> {
        let packet = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

            let list = match entry.group.steward_list() {
                Some(l) => l,
                None => return Ok(()),
            };

            let sync = StewardListSync {
                steward_members: list.members().to_vec(),
                start_epoch: list.start_epoch(),
                sn_min: list.config().sn_min as u32,
                sn_max: list.config().sn_max as u32,
            };

            let app_msg: AppMessage = sync.into();
            core::build_message(&entry.group, &self.mls_service, &app_msg, &self.app_id)?
        };

        self.handler.on_outbound(group_name, packet).await?;
        info!("[send_steward_list_sync] Sent steward list sync for group {group_name}");
        Ok(())
    }

    /// Check if any members are below the removal threshold and initiate ECPs.
    ///
    /// Only the steward initiates score-based removals. Skips self and any
    /// member for which a removal ECP is already pending.
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
                .filter(|id| *id != self_id) // skip self (deferred to M2)
                .map(|id| {
                    let score = scoring.score_for(group_name, &id).unwrap_or(0);
                    (id, score)
                })
                .collect()
        };

        // Batch: filter already-pending targets and mark new ones under a single lock.
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
                "Steward initiating SCORE_BELOW_THRESHOLD removal for member {:?} \
                 (score={current_score}) in group {group_name}",
                target_id
            );
            if let Err(e) = self
                .initiate_proposal(group_name.to_string(), request)
                .await
            {
                let mut groups = self.groups.write().await;
                if let Some(entry) = groups.get_mut(group_name) {
                    entry.group.resolve_pending_removal(&target_id);
                }
                error!(
                    "Failed to start SCORE_BELOW_THRESHOLD vote for {:?}: {e}",
                    target_id
                );
            }
        }

        Ok(())
    }
}
