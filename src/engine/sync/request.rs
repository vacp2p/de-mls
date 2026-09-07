//! The pull side of sync: asking for a `ConversationSync` and reporting the
//! miss once both answer turns pass. Entering and leaving `Syncing` are phase
//! transitions and live with the others.

use crate::{
    engine::{consensus::wire::control_bytes, handle::Engine, store::EngineStore, types::Event},
    protos::de_mls::messages::v1::{ConversationSyncRequest, control_message},
};

impl<St: EngineStore> Engine<St> {
    /// Broadcast a `ConversationSyncRequest` so a steward answers with its
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

    /// Report the miss: both answer turns of the request this node sent
    /// passed with no valid answer. Asking again is the router's
    /// (`Engine::request_sync`).
    pub(crate) fn drive_sync_request(&mut self) {
        if let Some(until) = self.timing.sync_deadline
            && self.now >= until
        {
            self.timing.sync_deadline = None;
            self.emit(Event::SyncUnanswered);
        }
    }
}
