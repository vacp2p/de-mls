//! The answer side of sync recovery: building and sharing the current
//! `ConversationSync`, and answering a `ConversationSyncRequest` — the epoch
//! steward at once, a backup steward after `backup_takeover_window` if the
//! epoch steward stays silent.

use tracing::info;

use crate::{
    ConversationError,
    engine::{consensus::wire::control_bytes, handle::Engine, store::EngineStore},
    protos::de_mls::messages::v1::{ConversationSync, PeerScore, TimingConfig, control_message},
};

impl<St: EngineStore> Engine<St> {
    /// The `ConversationSync` control bytes that carry the current steward
    /// list, protocol flags, timing and peer scores. `None` when there is no
    /// list yet; the caller owns delivery.
    pub(crate) fn build_conversation_sync(&mut self) -> Result<Option<Vec<u8>>, ConversationError> {
        // Sparse snapshot — only members whose score has diverged from
        // `default_score`, which rides along in `default_peer_score` so the
        // joiner adopts the same pivot and reads the omissions as we meant
        // them. Saves wire size at scale.
        let peer_scores: Vec<PeerScore> = self
            .scoring
            .snapshot()
            .diverged
            .into_iter()
            .map(|(member_id, score)| PeerScore { member_id, score })
            .collect();

        let Some(list) = self.steward_list.current_list() else {
            return Ok(None);
        };
        let steward_members = list.members().to_vec();
        let election_epoch = list.election_epoch();
        let sn_min = list.config().sn_min as u32;
        let sn_max = list.config().sn_max as u32;
        // `retry_round` is the fixed seed that made this list, frozen on the
        // list rather than the next dynamic attempt. Joiners use it to
        // recompute the list order.
        let retry_round = list.retry_round();

        // Members that weren't settled at election time. A joiner uses them to
        // reconstruct the candidate pool and re-derive the list itself.
        let unsettled_members: Vec<Vec<u8>> = self
            .members
            .iter()
            .filter(|m| !self.queues.is_settled(m, election_epoch))
            .cloned()
            .collect();

        let sync = ConversationSync {
            steward_members,
            election_epoch,
            sn_min,
            sn_max,
            peer_scores,
            default_peer_score: self.scoring.default_score(),
            timing: Some(TimingConfig::from(&self.config)),
            retry_round,
            liveness_criteria_yes: self.config.liveness_criteria_yes,
            unsettled_members,
        };
        Ok(Some(control_bytes(
            control_message::Payload::ConversationSync(sync),
        )))
    }

    /// Broadcast the current `ConversationSync` as an ordinary control
    /// message, for anyone with no list, or a stale one, to adopt. No-op
    /// while there is no list to share.
    pub(crate) fn share_conversation_sync(&mut self) -> Result<(), ConversationError> {
        if let Some(bytes) = self.build_conversation_sync()? {
            self.send_control(bytes);
        }
        Ok(())
    }

    /// Answer a sync re-send request. The epoch steward responds now; a backup
    /// arms `sync_resend_anchor` and takes over from `tick` only if the epoch
    /// steward stays silent. The list-present guard in
    /// `on_conversation_sync` means only a listless or exhausted node adopts
    /// the broadcast.
    pub(crate) fn on_conversation_sync_request(
        &mut self,
        requester: &[u8],
    ) -> Result<(), ConversationError> {
        // An exhausted list helps nobody; only a synced node answers.
        if !self.is_synced() {
            return Ok(());
        }
        if self.is_epoch_steward() {
            self.timing.sync_resend_anchor = None;
            tracing::debug!(
                conversation = %self.conversation_id,
                requester = ?requester,
                "epoch steward re-sending conversation sync"
            );
            return self.share_conversation_sync();
        }
        // A backup arms the takeover timer; a plain member can't answer.
        if self.is_steward() && self.timing.sync_resend_anchor.is_none() {
            self.timing.sync_resend_anchor = Some(self.now);
        }
        Ok(())
    }

    /// Backup-steward sync re-send takeover. The epoch steward answers a
    /// `ConversationSyncRequest` reactively; a backup arms `sync_resend_anchor`
    /// and re-sends here only if no `ConversationSync` was observed within
    /// `backup_takeover_window` — so an offline epoch steward can't strand a
    /// listless member. Only a synced backup steward drives it; the anchor
    /// clears otherwise.
    pub(crate) fn drive_sync_resend(&mut self) -> Result<(), ConversationError> {
        let Some(anchor) = self.timing.sync_resend_anchor else {
            return Ok(());
        };
        // Only a synced backup takes over: an exhausted list can't answer,
        // the epoch steward already did, and a plain member can't. Any of
        // these means the anchor no longer applies.
        if !self.is_synced() || self.is_epoch_steward() || !self.is_steward() {
            self.timing.sync_resend_anchor = None;
            return Ok(());
        }
        if self.now < anchor + self.config.backup_takeover_window {
            return Ok(());
        }
        self.timing.sync_resend_anchor = None;
        info!(
            conversation = %self.conversation_id,
            "backup steward re-sending conversation sync: epoch steward silent past backup_takeover_window"
        );
        self.share_conversation_sync()
    }
}
