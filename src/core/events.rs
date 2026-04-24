//! I/O contract between the protocol layer and an integrator. The app layer
//! ([`crate::app::User`]) calls these methods while dispatching
//! [`crate::core::ProcessResult`] variants — core itself never calls them.

use async_trait::async_trait;

use crate::ds::OutboundPacket;
use crate::protos::de_mls::messages::v1::{AppMessage, GroupUpdateRequest};

/// Error wrapper returned by [`GroupEventHandler`] callbacks. Integrators
/// convert their transport/UI errors into this via `CallbackError(e.to_string())`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CallbackError(pub String);

/// Integrator-supplied bridge between DE-MLS and the transport / UI.
///
/// Called from async contexts while managing multiple groups, hence `Send + Sync`.
#[async_trait]
pub trait GroupEventHandler: Send + Sync {
    /// Send an encrypted packet to the network. The packet's `subtopic`
    /// distinguishes welcome vs application traffic. Returns a transport
    /// message id if meaningful, else an empty string.
    async fn on_outbound(
        &self,
        group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CallbackError>;

    /// Deliver a decrypted application message (chat, vote request, proposal
    /// notification, ban request, …) to the UI.
    async fn on_app_message(
        &self,
        group_name: &str,
        message: AppMessage,
    ) -> Result<(), CallbackError>;

    /// The user is out of this group (self-leave commit merged, or someone
    /// else removed us). When using [`crate::app::User`] the registry is
    /// already pruned — use this only for UI/transport cleanup.
    async fn on_leave_group(&self, group_name: &str) -> Result<(), CallbackError>;

    /// Welcome processed and MLS state initialised. When using
    /// [`crate::app::User`] epoch timers + state transitions are already
    /// wired — use this only for UI notification.
    async fn on_joined_group(&self, group_name: &str) -> Result<(), CallbackError>;

    /// A background operation (e.g., vote submission) failed. Log and
    /// optionally surface to the UI.
    async fn on_error(&self, group_name: &str, operation: &str, error: &str);

    /// The local wallet just submitted `request` as a new proposal. The
    /// creator's vote is bundled with the outbound proposal and has already
    /// reached peers — the local UI should record the proposal for history
    /// rendering but must **not** surface a "please vote" affordance.
    /// Default impl is a no-op for integrators without a voting UI.
    async fn on_own_proposal_submitted(
        &self,
        _group_name: &str,
        _proposal_id: u32,
        _request: &GroupUpdateRequest,
    ) -> Result<(), CallbackError> {
        Ok(())
    }
}
