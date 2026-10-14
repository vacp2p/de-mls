//! The pull side of sync: asking for a `ConversationSync`. The engine asks
//! once and keeps no deadline; asking again is the router's. Entering and
//! leaving `Syncing` are phase transitions and live with the others.

use crate::{
    engine::{EngineStore, consensus::wire::control_bytes, handle::Engine},
    protos::de_mls::messages::v1::{ConversationSyncRequest, control_message},
};

impl<St: EngineStore> Engine<St> {
    /// Broadcast a `ConversationSyncRequest` so a steward answers with its
    /// `ConversationSync`. The message carries no fields: the router hands the
    /// authenticated sender over, and that is the requester. The engine asks
    /// once per join, stale restore and adoption, and keeps no deadline for
    /// the answer; asking again is the router's, through
    /// `Engine::request_sync`.
    pub(crate) fn broadcast_sync_request(&mut self) {
        let bytes = control_bytes(control_message::Payload::ConversationSyncRequest(
            ConversationSyncRequest {},
        ));
        self.send_control(bytes);
    }
}
