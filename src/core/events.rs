//! Event handler trait for group operations.

use ds::transport::OutboundPacket;
use hashgraph_like_consensus::protos::consensus::v1::{Proposal, Vote};

use crate::protos::de_mls::messages::v1::{AppMessage, UpdateRequest};

/// Trait for handling events from group operations.
///
/// Implementations of this trait receive callbacks when significant events
/// occur during group operations. This allows applications to customize
/// how they handle outbound messages, application events, and state changes.
///
/// # Example
///
/// ```ignore
/// use de_mls::core::GroupEventHandler;
///
/// struct MyHandler {
///     delivery_service: MyDeliveryService,
///     ui_sender: MySender,
/// }
///
/// impl GroupEventHandler for MyHandler {
///     fn on_outbound(&self, group_name: &str, packet: OutboundPacket) {
///         self.delivery_service.send(packet);
///     }
///
///     fn on_app_message(&self, group_name: &str, message: AppMessage) {
///         self.ui_sender.send(message);
///     }
///     // ... other methods
/// }
/// ```
pub trait GroupEventHandler: Send + Sync {
    /// Called when an outbound packet needs to be sent to the network.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group this packet is for
    /// * `packet` - The outbound packet to send
    fn on_outbound(&self, group_name: &str, packet: OutboundPacket);

    /// Called when an application message is received from the group.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group this message is from
    /// * `message` - The application message
    fn on_app_message(&self, group_name: &str, message: AppMessage);

    /// Called when the user should leave the group.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group to leave
    fn on_leave_group(&self, group_name: &str);

    /// Called when a consensus proposal is received.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group this proposal is for
    /// * `proposal` - The consensus proposal
    fn on_proposal_received(&self, group_name: &str, proposal: Proposal);

    /// Called when a consensus vote is received.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group this vote is for
    /// * `vote` - The consensus vote
    fn on_vote_received(&self, group_name: &str, vote: Vote);

    /// Called when a member proposal is added (steward received a key package).
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `request` - The update request representing the proposal
    fn on_member_proposal_added(&self, group_name: &str, request: UpdateRequest);

    /// Called when the user successfully joins a group.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group joined
    fn on_joined_group(&self, group_name: &str);
}
