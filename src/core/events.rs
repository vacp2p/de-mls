//! I/O interface for group operations.
//!
//! [`GroupEventHandler`] is the I/O contract between the protocol layer and
//! your application. It is defined in core because it references core types
//! ([`crate::ds::OutboundPacket`], [`crate::protos::de_mls::messages::v1::AppMessage`]),
//! but it is **called by the app layer** (e.g. [`crate::app::User`]) after
//! dispatching a [`crate::core::ProcessResult`] — core itself never calls it.

use async_trait::async_trait;

use crate::ds::OutboundPacket;
use crate::protos::de_mls::messages::v1::AppMessage;

/// Error returned by [`GroupEventHandler`] callback implementations.
///
/// Wrap your transport or UI errors with this type to signal failure
/// back to the calling layer.
///
/// # Example
///
/// ```ignore
/// async fn on_outbound(&self, _: &str, packet: OutboundPacket) -> Result<String, CallbackError> {
///     self.transport.send(packet)
///         .map_err(|e| CallbackError(e.to_string()))
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CallbackError(pub String);

/// I/O interface for connecting group operations to transport and UI.
///
/// Implement this trait to wire DE-MLS into your application. The app layer
/// (e.g. [`crate::app::User`]) calls these methods when dispatching
/// [`crate::core::ProcessResult`] variants — they are never called by core
/// functions directly.
///
/// # Thread Safety
///
/// Requires `Send + Sync` because the app layer may call callbacks from
/// async contexts while managing multiple groups concurrently.
///
/// # Example
///
/// ```ignore
/// use async_trait::async_trait;
/// use de_mls::core::{CallbackError, GroupEventHandler};
/// use de_mls::protos::de_mls::messages::v1::AppMessage;
/// use de_mls::ds::OutboundPacket;
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
///     ) -> Result<String, CallbackError> {
///         self.transport.send(packet).await
///             .map_err(|e| CallbackError(e.to_string()))
///     }
///
///     async fn on_app_message(
///         &self,
///         group_name: &str,
///         message: AppMessage,
///     ) -> Result<(), CallbackError> {
///         self.ui_sender.send(UiEvent::Message { group_name, message }).await
///             .map_err(|e| CallbackError(e.to_string()))
///     }
///
///     async fn on_leave_group(&self, group_name: &str) -> Result<(), CallbackError> {
///         self.ui_sender.send(UiEvent::GroupRemoved(group_name.to_string())).await
///             .map_err(|e| CallbackError(e.to_string()))
///     }
///
///     async fn on_joined_group(&self, group_name: &str) -> Result<(), CallbackError> {
///         self.ui_sender.send(UiEvent::GroupJoined(group_name.to_string())).await
///             .map_err(|e| CallbackError(e.to_string()))
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
    /// - Failures should be returned as `CallbackError`
    async fn on_outbound(
        &self,
        group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CallbackError>;

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
    async fn on_app_message(
        &self,
        group_name: &str,
        message: AppMessage,
    ) -> Result<(), CallbackError>;

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
    /// - When using [`crate::app::User`], the group is already removed from its
    ///   internal registry before this callback fires — use it only for UI/transport
    ///   cleanup (unsubscribe topics, show "removed" notification, etc.)
    /// - When building a custom app layer, remove the group from your own registry here.
    async fn on_leave_group(&self, group_name: &str) -> Result<(), CallbackError>;

    /// Called when the user successfully joined a group.
    ///
    /// This is called after a welcome message is processed and the MLS
    /// state is initialized.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group joined
    ///
    /// # Implementation Notes
    /// - When using [`crate::app::User`], epoch synchronization and state machine
    ///   transitions are handled automatically — use this only for UI notification.
    /// - When building a custom app layer, start epoch timers and transition state here.
    async fn on_joined_group(&self, group_name: &str) -> Result<(), CallbackError>;

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
