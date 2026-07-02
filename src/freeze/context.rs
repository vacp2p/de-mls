//! Pre-merge round snapshot. [`RoundContext`] is the eligibility-filtered view
//! of the freeze round that per-candidate apply reads from; `snapshot` builds it
//! once, before the apply loop runs.

use crate::{ConversationError, ConversationQueues, StewardListService, mls_crypto::MlsService};

/// Pre-merge round state used during candidate apply.
pub(super) struct RoundContext {
    /// Number of MLS-producing approvals (Add/Remove). Used to filter
    /// candidates whose proposal count doesn't match the voted set.
    pub(super) mls_count: usize,
    /// Flips the local apply's terminal result to `LeaveConversation`
    /// when our removal is in the batch.
    pub(super) self_remove_pending: bool,
    pub(super) current_epoch: u64,
    /// RFC §Layer 3 Anti-Deadlock: any member MAY commit when set;
    /// otherwise the commit sender must be on the steward list.
    pub(super) in_recovery: bool,
    /// Pre-merge eligibility-filtered steward expected at
    /// `current_epoch`. Used to penalise an absent steward when a
    /// backup commits in their place.
    pub(super) epoch_steward_id: Option<Vec<u8>>,
}

impl RoundContext {
    pub(super) fn snapshot(
        conversation: &ConversationQueues,
        mls: &MlsService,
        steward_list: &StewardListService,
        current_epoch: u64,
        in_recovery: bool,
        self_member_id: &[u8],
    ) -> Result<Self, ConversationError> {
        let mls_count = conversation
            .approved_proposals()
            .values()
            .filter(|req| req.produces_mls_action())
            .count();
        let self_remove_pending = conversation.has_approved_removal(self_member_id);

        let members = mls.members()?;
        let eligible = conversation.steward_eligibility(&members);
        let epoch_steward_id = steward_list
            .epoch_steward(current_epoch, &eligible)
            .map(|s| s.to_vec());

        Ok(Self {
            mls_count,
            self_remove_pending,
            current_epoch,
            in_recovery,
            epoch_steward_id,
        })
    }
}
