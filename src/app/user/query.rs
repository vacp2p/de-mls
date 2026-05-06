//! Read-only queries over a group's state (UI and diagnostics).

use crate::{
    app::{GroupState, MemberRole, StateChangeHandler, User, UserError},
    core::{DeMlsProvider, GroupEventHandler, group_members},
    mls_crypto::{MlsService, format_wallet_address},
    protos::de_mls::messages::v1::GroupUpdateRequest,
};

impl<
    P: DeMlsProvider,
    M: MlsService,
    H: GroupEventHandler + 'static,
    SCH: StateChangeHandler + 'static,
> User<P, M, H, SCH>
where
    M::Identity: Clone,
{
    pub async fn get_group_state(&self, group_name: &str) -> Result<GroupState, UserError> {
        self.with_entry(group_name, |e| e.state_machine.current_state())
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
        let epoch = match entry.group.mls() {
            Some(mls) => mls.current_epoch()?,
            None => 0,
        };
        Ok((epoch, entry.group.reelection_round()))
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
            let expected = e.group.steward_list().map(|l| l.len()).unwrap_or(0);
            (received, expected)
        })
        .await
        .ok_or(UserError::GroupNotFound)
    }

    pub async fn is_steward_for_group(&self, group_name: &str) -> Result<bool, UserError> {
        self.with_entry(group_name, |e| e.group.is_steward())
            .await
            .ok_or(UserError::GroupNotFound)
    }

    pub async fn get_group_members(&self, group_name: &str) -> Result<Vec<String>, UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let entry = entry_arc.read().await;
        if entry.group.mls().is_none() {
            return Ok(Vec::new());
        }
        let members = group_members(&entry.group)?;
        Ok(members
            .into_iter()
            .map(|raw| format_wallet_address(raw.as_slice()).to_string())
            .collect())
    }

    pub fn get_member_scores(&self, group_name: &str) -> Vec<(Vec<u8>, i64)> {
        self.scoring().all_members_with_scores(group_name)
    }

    pub fn get_member_score(&self, group_name: &str, member_id: &[u8]) -> Option<i64> {
        self.scoring().score_for(group_name, member_id)
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
        if entry.group.mls().is_none() {
            return Err(UserError::MlsNotInitialized);
        }
        let members = group_members(&entry.group)?;
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
        let mls = entry.group.mls().ok_or(UserError::MlsNotInitialized)?;
        let epoch = mls.current_epoch()?;
        let members = group_members(&entry.group)?;

        let list = entry.group.steward_list();
        let (live_epoch, live_backup) = entry.group.live_epoch_and_backup(epoch, &members);
        let roles = members
            .iter()
            .cloned()
            .map(|id| {
                let role = match list {
                    Some(l) if !l.is_exhausted(epoch) => {
                        if live_epoch.is_some_and(|es| es == id) {
                            MemberRole::EpochSteward
                        } else if live_backup.is_some_and(|bs| bs == id) {
                            MemberRole::BackupSteward
                        } else if l.contains(&id) {
                            MemberRole::Steward
                        } else {
                            MemberRole::Member
                        }
                    }
                    Some(l) if l.contains(&id) => MemberRole::Steward,
                    _ => MemberRole::Member,
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

    pub async fn get_epoch_history(
        &self,
        group_name: &str,
    ) -> Result<Vec<Vec<GroupUpdateRequest>>, UserError> {
        self.with_entry(group_name, |e| {
            e.epoch_history
                .iter()
                .map(|batch| batch.values().cloned().collect())
                .collect()
        })
        .await
        .ok_or(UserError::GroupNotFound)
    }
}
