//! Time-driven steward takeovers: the backup drain for the sync re-send, and
//! the pull side of sync recovery.

use tracing::info;

use crate::{
    ConversationError,
    engine::{handle::Engine, store::EngineStore, types::Event},
};

impl<St: EngineStore> Engine<St> {
    /// Backup-steward sync re-send takeover. The epoch steward answers a
    /// `ConversationSyncRequest` reactively; a backup arms `sync_resend_anchor`
    /// and re-sends here only if no `ConversationSync` was observed within
    /// `backup_takeover_window` — so an offline epoch steward can't strand a
    /// bootstrap-less joiner. Only a backup steward drives it; the anchor
    /// clears otherwise.
    pub(crate) fn drive_sync_resend(&mut self) -> Result<(), ConversationError> {
        let Some(anchor) = self.timing.sync_resend_anchor else {
            return Ok(());
        };
        // Only a backup takes over: the epoch steward already answered, and a
        // plain member can't. Either way the anchor no longer applies.
        if self.is_epoch_steward() || !self.is_steward() {
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

    /// Pull side of sync recovery: when the local steward list is missing or
    /// exhausted at the current epoch and no election is being voted on
    /// locally, ask for the current list. Covers anyone that missed an
    /// election — a joiner welcomed that epoch, or a steward returning from
    /// offline whose stale list still names it. Rate-limited to one request
    /// per `backup_takeover_window` (the answer latency). Skipped while an
    /// election is in flight, which resolves into the list directly.
    pub(crate) fn drive_sync_request(&mut self) -> Result<(), ConversationError> {
        let needs_sync = match self.steward_list.current_list() {
            None => true,
            Some(_) => self.steward_list.is_exhausted(self.epoch),
        };
        if !needs_sync || self.queues.has_election_in_flight() {
            self.timing.sync_request_anchor = None;
            self.timing.unanswered_sync_rounds = 0;
            return Ok(());
        }
        if let Some(anchor) = self.timing.sync_request_anchor
            && self.now < anchor + self.config.backup_takeover_window
        {
            return Ok(());
        }
        self.timing.sync_request_anchor = Some(self.now);
        self.timing.unanswered_sync_rounds += 1;
        let alert_at = self.config.unanswered_sync_rounds;
        if alert_at != 0 && self.timing.unanswered_sync_rounds == alert_at {
            self.emit(Event::SyncUnanswered);
        }
        info!(
            conversation = %self.conversation_id,
            rounds = self.timing.unanswered_sync_rounds,
            "requesting conversation sync: local steward list missing or exhausted"
        );
        self.broadcast_sync_request();
        Ok(())
    }
}
