//! The answer side of sync: building and sending the current
//! `ConversationSync`, and answering a `ConversationSyncRequest`: the epoch
//! steward at once, a backup steward after `backup_takeover_window` if the
//! epoch steward stays silent.

use tracing::info;

use crate::{
    engine::{consensus::wire::control_bytes, handle::Engine, store::EngineStore},
    protos::de_mls::messages::v1::{
        ConversationSync, JoinEpoch, PeerScore, TimingConfig, control_message,
    },
};

impl<St: EngineStore> Engine<St> {
    /// The `ConversationSync` control bytes carrying the current steward
    /// list, protocol flags, timing and peer scores. `None` while there is no
    /// list; the caller owns delivery.
    pub(crate) fn build_conversation_sync(&self) -> Option<Vec<u8>> {
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

        let list = self.steward_list.current_list()?;
        let steward_members = list.members().to_vec();
        let election_epoch = list.election_epoch();
        let sn_min = list.config().sn_min as u32;
        let sn_max = list.config().sn_max as u32;
        // `retry_round` is the fixed seed that made this list, frozen on the
        // list rather than the next dynamic attempt. Joiners use it to
        // recompute the list order.
        let retry_round = list.retry_round();

        // Every join epoch this node holds for a current member. A joiner
        // rebuilds the candidate pool at election time from them and
        // re-derives the list itself, and keeps them for later elections.
        let join_epochs: Vec<JoinEpoch> = self
            .members
            .iter()
            .filter_map(|m| {
                self.queues.join_epoch(m).map(|epoch| JoinEpoch {
                    member_id: m.clone(),
                    epoch,
                })
            })
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
            join_epochs,
        };
        Some(control_bytes(control_message::Payload::ConversationSync(
            sync,
        )))
    }

    /// Send the current `ConversationSync` as a control message. No-op while
    /// there is no list to share.
    pub(crate) fn share_conversation_sync(&mut self) {
        if let Some(bytes) = self.build_conversation_sync() {
            self.send_control(bytes);
        }
    }

    /// Answer a sync request. The epoch steward answers now; a backup
    /// steward arms `sync_takeover_anchor` and answers from `tick` only if
    /// the epoch steward stays silent; a plain member cannot answer. Only a
    /// synced node answers: an exhausted list helps nobody.
    pub(crate) fn on_conversation_sync_request(&mut self, requester: &[u8]) {
        if !self.is_synced() {
            return;
        }
        if self.is_epoch_steward() {
            self.timing.sync_takeover_anchor = None;
            tracing::debug!(
                conversation = %self.conversation_id,
                requester = ?requester,
                "epoch steward answering sync request"
            );
            self.share_conversation_sync();
            return;
        }
        // A backup arms the takeover timer; a plain member can't answer.
        if self.is_steward() && self.timing.sync_takeover_anchor.is_none() {
            self.timing.sync_takeover_anchor = Some(self.now);
        }
    }

    /// The backup steward's answer turn. A backup arms `sync_takeover_anchor`
    /// when a request arrives and answers here only if no `ConversationSync`
    /// was seen within `backup_takeover_window`, so a silent epoch steward
    /// cannot strand a requester. The anchor clears when this node is not a
    /// synced backup steward.
    pub(crate) fn drive_sync_takeover(&mut self) {
        let Some(anchor) = self.timing.sync_takeover_anchor else {
            return;
        };
        // Only a synced backup takes over: an exhausted list can't answer,
        // the epoch steward already did, and a plain member can't. Any of
        // these means the anchor no longer applies.
        if !self.is_synced() || self.is_epoch_steward() || !self.is_steward() {
            self.timing.sync_takeover_anchor = None;
            return;
        }
        if self.now < anchor + self.config.backup_takeover_window {
            return;
        }
        self.timing.sync_takeover_anchor = None;
        info!(
            conversation = %self.conversation_id,
            "backup steward answering sync request: epoch steward silent past backup_takeover_window"
        );
        self.share_conversation_sync();
    }
}
