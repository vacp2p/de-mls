//! Event handler trait for group operations.
//!
//! The [`GroupEventHandler`] trait is the primary integration point between
//! the DE-MLS core and your application. Implement this trait to handle
//! output events from group operations.

use async_trait::async_trait;

use crate::core::error::CoreError;
use crate::ds::OutboundPacket;
use crate::protos::de_mls::messages::v1::AppMessage;

/// Trait for handling output events from group operations.
///
/// This is the main trait you need to implement to integrate DE-MLS with
/// your application. It receives callbacks for all significant output events:
///
/// - Network packets that need to be sent
/// - Application messages for UI display
/// - Group membership changes (join/leave)
/// - Background operation errors
///
/// # Thread Safety
///
/// This trait requires `Send + Sync` because callbacks may be invoked from
/// async contexts and multiple groups may be processed concurrently.
///
/// # Example
///
/// ```ignore
/// use async_trait::async_trait;
/// use de_mls::core::{GroupEventHandler, CoreError};
/// use de_mls::protos::de_mls::messages::v1::AppMessage;
/// use ds::transport::OutboundPacket;
///
/// struct MyHandler {
///     transport: MyTransport,
///     ui_sender: mpsc::Sender<UiEvent>,
/// }
///
/// #[async_trait]
/// impl GroupEventHandler for MyHandler {
///     async fn on_outbound(
///         &self,
///         group_name: &str,
///         packet: OutboundPacket,
///     ) -> Result<String, CoreError> {
///         self.transport.send(packet).await
///             .map_err(|e| CoreError::DeliveryError(e.to_string()))
///     }
///
///     async fn on_app_message(
///         &self,
///         group_name: &str,
///         message: AppMessage,
///     ) -> Result<(), CoreError> {
///         self.ui_sender.send(UiEvent::Message { group_name, message }).await
///             .map_err(|e| CoreError::HandlerError(e.to_string()))
///     }
///
///     async fn on_leave_group(&self, group_name: &str) -> Result<(), CoreError> {
///         self.ui_sender.send(UiEvent::GroupRemoved(group_name.to_string())).await
///             .map_err(|e| CoreError::HandlerError(e.to_string()))
///     }
///
///     async fn on_joined_group(&self, group_name: &str) -> Result<(), CoreError> {
///         self.ui_sender.send(UiEvent::GroupJoined(group_name.to_string())).await
///             .map_err(|e| CoreError::HandlerError(e.to_string()))
///     }
///
///     async fn on_error(&self, group_name: &str, operation: &str, error: &str) {
///         tracing::error!("Error in {operation} for group {group_name}: {error}");
///     }
/// }
/// ```
#[async_trait]
pub trait GroupEventHandler: Send + Sync {
    /// Called when a packet needs to be sent to the network.
    ///
    /// This is the primary output for MLS-encrypted messages. The packet
    /// contains the encrypted payload, subtopic, group name, and app ID.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group this packet is for
    /// * `packet` - The outbound packet to send
    ///
    /// # Returns
    /// A message ID or identifier from the transport layer (if available).
    ///
    /// # Implementation Notes
    /// - This should send the packet via your transport layer (Waku, libp2p, etc.)
    /// - The packet's `subtopic` determines the message type (welcome vs app)
    /// - Failures should be returned as `CoreError::DeliveryError`
    async fn on_outbound(
        &self,
        group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CoreError>;

    /// Called when an application message should be delivered to the UI.
    ///
    /// This includes:
    /// - Chat messages (`ConversationMessage`)
    /// - Vote requests (`VotePayload`)
    /// - Proposal notifications (`ProposalAdded`)
    /// - Ban requests (`BanRequest`)
    ///
    /// # Arguments
    /// * `group_name` - The name of the group this message is from
    /// * `message` - The application message (see `app_message::Payload` variants)
    ///
    /// # Implementation Notes
    /// - Dispatch based on `message.payload` variant
    /// - `VotePayload` should trigger UI for user to approve/reject
    /// - `ConversationMessage` should be displayed in chat
    async fn on_app_message(&self, group_name: &str, message: AppMessage) -> Result<(), CoreError>;

    /// Called when the user has been removed from a group.
    ///
    /// This is called for both:
    /// - Voluntary leave (after the removal commit is processed)
    /// - Forced removal (when another member removes you)
    ///
    /// # Arguments
    /// * `group_name` - The name of the group to leave
    ///
    /// # Implementation Notes
    /// - Remove the group from your registry
    /// - Stop any background tasks for this group
    /// - Notify the UI that the group was removed
    async fn on_leave_group(&self, group_name: &str) -> Result<(), CoreError>;

    /// Called when the user successfully joined a group.
    ///
    /// This is called after a welcome message is processed and the MLS
    /// state is initialized.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group joined
    ///
    /// # Implementation Notes
    /// - Update UI to show the user is now a member
    /// - Start any background tasks for this group (epoch timer, etc.)
    async fn on_joined_group(&self, group_name: &str) -> Result<(), CoreError>;

    /// Called when a background operation fails.
    ///
    /// This is used for operations that run in spawned tasks, such as
    /// voting requests, where errors can't be returned directly.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `operation` - Description of the failed operation (e.g., "start_voting")
    /// * `error` - The error message
    ///
    /// # Implementation Notes
    /// - Log the error
    /// - Optionally notify the UI
    async fn on_error(&self, group_name: &str, operation: &str, error: &str);
}
