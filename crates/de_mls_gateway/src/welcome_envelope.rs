//! Wire envelope for gateway-published welcomes on the welcome
//! subtopic. Wraps a `MemberWelcome` so the receiver can tell it apart
//! from a bare `MemberInvite` published by the library's
//! `send_key_package` — envelope decode succeeds for welcomes; raw
//! `MemberInvite` bytes fall through (the receiver forwards them
//! untouched to `User::process_inbound_packet`).
//!
//! Defined here rather than in `de_mls` because welcome delivery is the
//! integrator's responsibility, not the protocol's.

use de_mls::protos::de_mls::messages::v1::MemberWelcome;
use prost::Message;

#[derive(Clone, PartialEq, prost::Message)]
pub struct WelcomeSubtopicEnvelope {
    #[prost(message, optional, tag = "2")]
    pub welcome: Option<MemberWelcome>,
}

/// Encode an outbound welcome envelope.
pub fn encode_welcome(welcome: MemberWelcome) -> Vec<u8> {
    WelcomeSubtopicEnvelope {
        welcome: Some(welcome),
    }
    .encode_to_vec()
}

/// Decode a welcome-subtopic payload. Returns `Some(welcome)` for an
/// envelope produced by another gateway; `None` for any other bytes
/// (typically a bare `MemberInvite` from `send_key_package`).
pub fn decode(bytes: &[u8]) -> Option<MemberWelcome> {
    WelcomeSubtopicEnvelope::decode(bytes).ok()?.welcome
}
