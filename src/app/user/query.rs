//! Read-only queries over a group's state (UI and diagnostics).

use crate::{
    app::{GroupState, MemberRole, StateChangeHandler, User, UserError},
    core::{DeMlsProvider, GroupEventHandler, PeerScoringPlugin, StewardListPlugin},
    identity::{Identity, format_wallet_address},
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::GroupUpdateRequest,
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
    pub async fn get_group_state(&self, group_name: &str) -> Result<GroupState, UserError> {
        self.with_entry(group_name, |e| e.current_state())
            .await
            .ok_or(UserError::GroupNotFound)
    }

    /// Current MLS epoch + reelection retry round. `(0, 0)` if the group has
    /// no MLS state yet (pending join). Intended for UI status display.
    pub async fn get_epoch_and_retry(&self, group_name: &str) -> Result<(u64, u32), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let entry = entry_arc.read().await;
        let epoch = match entry.mls() {
            Some(mls) => mls.current_epoch()?,
            None => 0,
        };
        Ok((epoch, entry.steward.retry_round()))
    }

    pub async fn list_groups(&self) -> Vec<String> {
        self.groups.read().await.keys().cloned().collect()
    }

    /// Count of buffered pending membership updates. Used by tests and the UI
    /// to verify buffer hygiene (e.g., that a joiner's buffer is empty right
    /// after they receive the welcome).
    pub async fn get_pending_update_count(&self, group_name: &str) -> Result<usize, UserError> {
        self.with_entry(group_name, |e| e.group.pending_update_count())
            .await
            .ok_or(UserError::GroupNotFound)
    }

    /// Freeze round progress: `(received, expected)`. Returns `(0, 0)` if not
    /// in freeze or no steward list is known.
    pub async fn get_freeze_candidate_count(
        &self,
        group_name: &str,
    ) -> Result<(usize, usize), UserError> {
        self.with_entry(group_name, |e| {
            let received = e.group.freeze_candidate_count();
            let expected = e.steward.current_list().map(|l| l.len()).unwrap_or(0);
            (received, expected)
        })
        .await
        .ok_or(UserError::GroupNotFound)
    }

    pub async fn is_steward_for_group(&self, group_name: &str) -> Result<bool, UserError> {
        let self_id = self.identity().identity_bytes().to_vec();
        self.with_entry(group_name, |e| e.steward.is_steward(&self_id))
            .await
            .ok_or(UserError::GroupNotFound)
    }

    pub async fn get_group_members(&self, group_name: &str) -> Result<Vec<String>, UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let entry = entry_arc.read().await;
        if entry.mls().is_none() {
            return Ok(Vec::new());
        }
        let members = entry.group_members()?;
        Ok(members
            .into_iter()
            .map(|raw| format_wallet_address(raw.as_slice()).to_string())
            .collect())
    }

    pub async fn get_member_scores(&self, group_name: &str) -> Vec<(Vec<u8>, i64)> {
        match self.lookup_entry(group_name).await {
            Some(entry_arc) => entry_arc.read().await.scoring.all_members_with_scores(),
            None => Vec::new(),
        }
    }

    pub async fn get_member_score(&self, group_name: &str, member_id: &[u8]) -> Option<i64> {
        let entry_arc = self.lookup_entry(group_name).await?;
        let entry = entry_arc.read().await;
        entry.scoring.score_for(member_id)
    }

    /// Identities that have an in-flight self-leave request. Used by the UI
    /// to render a "pending leave" indicator.
    pub async fn get_pending_leave_identities(
        &self,
        group_name: &str,
    ) -> Result<Vec<Vec<u8>>, UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let entry = entry_arc.read().await;
        entry.expect_mls()?;
        let members = entry.group_members()?;
        Ok(members
            .into_iter()
            .filter(|id| entry.group.is_pending_self_leave(id))
            .collect())
    }

    /// Steward role for each member. Uses live rotation so removed or
    /// pending-leave stewards are skipped in role display.
    pub async fn get_member_roles(
        &self,
        group_name: &str,
    ) -> Result<Vec<(Vec<u8>, MemberRole)>, UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let entry = entry_arc.read().await;
        let mls = entry.expect_mls()?;
        let epoch = mls.current_epoch()?;
        let members = entry.group_members()?;

        let eligible = entry.group.steward_eligibility(&members);
        let (live_epoch, live_backup) = entry.steward.epoch_and_backup(epoch, &eligible);
        let live_epoch = live_epoch.map(|s| s.to_vec());
        let live_backup = live_backup.map(|s| s.to_vec());
        let exhausted = entry.steward.is_exhausted(epoch);
        let has_list = entry.steward.current_list().is_some();
        let roles = members
            .iter()
            .cloned()
            .map(|id| {
                let role = if has_list && !exhausted {
                    if live_epoch.as_deref().is_some_and(|es| es == id) {
                        MemberRole::EpochSteward
                    } else if live_backup.as_deref().is_some_and(|bs| bs == id) {
                        MemberRole::BackupSteward
                    } else if entry.steward.is_steward(&id) {
                        MemberRole::Steward
                    } else {
                        MemberRole::Member
                    }
                } else if has_list && entry.steward.is_steward(&id) {
                    MemberRole::Steward
                } else {
                    MemberRole::Member
                };
                (id, role)
            })
            .collect();
        Ok(roles)
    }

    pub async fn get_approved_proposal_for_current_epoch(
        &self,
        group_name: &str,
    ) -> Result<Vec<GroupUpdateRequest>, UserError> {
        self.with_entry(group_name, |e| {
            e.group.approved_proposals().values().cloned().collect()
        })
        .await
        .ok_or(UserError::GroupNotFound)
    }
}
