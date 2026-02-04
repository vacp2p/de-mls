//! Event handler trait for group operations.

use async_trait::async_trait;
use ds::transport::OutboundPacket;

use super::error::CoreError;
use crate::protos::de_mls::messages::v1::AppMessage;

/// Trait for handling output events from group operations.
///
/// Implementations of this trait receive callbacks when significant output events
/// occur during group operations. This allows applications to customize
/// how they handle outbound messages, application events, and state changes.
///
/// This trait contains only *output* events (4 methods). Proposal/vote processing
/// is handled at the application layer via `ProcessResult` matching, since it
/// requires consensus service + state machine access.
///
/// # Example
#[async_trait]
pub trait GroupEventHandler: Send + Sync {
    /// Called when a packet needs to be sent to the network.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group this packet is for
    /// * `packet` - The outbound packet to send
    ///
    /// # Returns
    /// * message id (if available)
    async fn on_outbound(
        &self,
        group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CoreError>;

    /// Called when an application message should be delivered to the UI/consumer.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group this message is from
    /// * `message` - The application message
    async fn on_app_message(&self, group_name: &str, message: AppMessage) -> Result<(), CoreError>;

    /// Called when the user has been removed from a group.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group to leave
    async fn on_leave_group(&self, group_name: &str) -> Result<(), CoreError>;

    /// Called when the user successfully joined a group.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group joined
    async fn on_joined_group(&self, group_name: &str) -> Result<(), CoreError>;
}
