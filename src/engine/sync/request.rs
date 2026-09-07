//! The pull side of sync recovery: asking for a `ConversationSync` and
//! reporting the miss once both answer turns pass. Entering and leaving
//! `Syncing` are phase transitions and live with the others.

use crate::{
    ConversationError,
    engine::{
        consensus::wire::control_bytes,
        handle::Engine,
        store::EngineStore,
        types::{Event, Phase},
    },
    protos::de_mls::messages::v1::{ConversationSyncRequest, control_message},
};

impl<St: EngineStore> Engine<St> {
    /// Broadcast a `ConversationSyncRequest` so a steward re-sends its
    /// `ConversationSync`. The message carries no fields: the router hands the
    /// authenticated sender over, and that is the requester. The answer has
    /// two turns — the epoch steward's at once, a backup's after
    /// `backup_takeover_window` — so both windows are armed as one deadline
    /// and reported as the wakeup; if both pass unanswered, `SyncUnanswered`
    /// follows. The window is this node's own reading of
    /// `backup_takeover_window`: a joiner runs the default until the sync
    /// arrives.
    pub(crate) fn broadcast_sync_request(&mut self) {
        let bytes = control_bytes(control_message::Payload::ConversationSyncRequest(
            ConversationSyncRequest {},
        ));
        self.send_control(bytes);
        self.timing.sync_deadline = Some(self.now + self.config.backup_takeover_window * 2);
    }

    /// Report the miss: both answer turns of the request this node sent have
    /// passed with no sync adopted. Asking again is the router's
    /// (`Engine::request_sync`). Outside `Syncing` nothing is awaited.
    pub(crate) fn drive_sync_request(&mut self) -> Result<(), ConversationError> {
        if self.phase != Phase::Syncing {
            self.timing.sync_deadline = None;
            return Ok(());
        }
        if let Some(until) = self.timing.sync_deadline
            && self.now >= until
        {
            self.timing.sync_deadline = None;
            self.emit(Event::SyncUnanswered);
        }
        Ok(())
    }
}
