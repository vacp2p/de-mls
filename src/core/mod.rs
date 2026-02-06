//! Core library for DE-MLS group management.
//!
//! This module provides the reusable API for building decentralized MLS chat applications.
//! It handles MLS cryptography, consensus voting, and message routing while leaving
//! transport, UI, and state management to the application layer.
//!
//! # Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Your Application                        │
//! │  ┌─────────────┐  ┌──────────────┐  ┌───────────────────┐  │
//! │  │ Transport   │  │ UI/State     │  │ GroupEventHandler │  │
//! │  │ (Waku, etc) │  │ Management   │  │ (your impl)       │  │
//! │  └──────┬──────┘  └──────┬───────┘  └─────────┬─────────┘  │
//! └─────────┼────────────────┼────────────────────┼────────────┘
//!           │                │                    │
//! ┌─────────┼────────────────┼────────────────────┼────────────┐
//! │         ▼                ▼                    ▼            │
//! │  ┌─────────────────────────────────────────────────────┐   │
//! │  │                    de_mls::core                     │   │
//! │  │  • Group lifecycle (create, join, leave)            │   │
//! │  │  • Message encryption/decryption                    │   │
//! │  │  • Consensus voting integration                     │   │
//! │  │  • Proposal management                              │   │
//! │  └─────────────────────────────────────────────────────┘   │
//! │                           │                                │
//! │         ┌─────────────────┼─────────────────┐              │
//! │         ▼                 ▼                 ▼              │
//! │  ┌────────────┐   ┌───────────────┐   ┌────────────────┐   │
//! │  │ mls_crypto │   │ hashgraph-like│   │ Protobuf msgs  │   │
//! │  │ (OpenMLS)  │   │  consensus    │   │ (de_mls.v1)    │   │
//! │  └────────────┘   └───────────────┘   └────────────────┘   │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! # What You Implement
//!
//! To build a chat application, you need to implement:
//!
//! 1. **[`GroupEventHandler`]** - Required trait for handling output events:
//!    - `on_outbound()` - Send encrypted packets to the network
//!    - `on_app_message()` - Deliver messages to your UI
//!    - `on_leave_group()` / `on_joined_group()` - Update your state
//!    - `on_error()` - Handle background operation failures
//!
//! 2. **Transport Layer** - How packets travel between peers (Waku, libp2p, etc.)
//!
//! 3. **State Management** - Track which groups exist, their states, UI updates
//!
//! # What's Provided
//!
//! - **MLS Operations** - Group creation, joining, message encryption via OpenMLS
//! - **Consensus Voting** - Proposal creation, vote casting, result handling
//! - **Message Types** - Protobuf definitions for all protocol messages
//! - **Default Services** - [`DefaultProvider`] bundles standard implementations
//!
//! # Core Concepts
//!
//! ## GroupHandle
//!
//! [`GroupHandle`] is the per-group state container. It holds:
//! - MLS cryptographic state (encrypted, thread-safe via `Arc<Mutex>`)
//! - Steward status (who can commit membership changes)
//! - Pending and approved proposals for the current epoch
//!
//! ## Steward Role
//!
//! One member per group is the "steward" who:
//! - Receives key packages from users wanting to join
//! - Batches approved proposals into MLS commits
//! - Sends welcome messages to new members
//!
//! ## ProcessResult & DispatchAction
//!
//! When processing inbound messages:
//! 1. Call [`process_inbound()`] → returns [`ProcessResult`]
//! 2. Call [`dispatch_result()`] → returns [`DispatchAction`]
//! 3. Handle the action in your application layer
//!
//! # Quick Start
//!
//! ```ignore
//! use de_mls::core::{
//!     create_group, build_message, process_inbound, dispatch_result,
//!     GroupEventHandler, GroupHandle, ProcessResult, DispatchAction,
//!     DefaultProvider,
//! };
//!
//! // 1. Implement GroupEventHandler
//! struct MyHandler { /* transport, ui channels */ }
//!
//! #[async_trait]
//! impl GroupEventHandler for MyHandler {
//!     async fn on_outbound(&self, group: &str, packet: OutboundPacket) -> Result<String, CoreError> {
//!         self.transport.send(packet).await
//!     }
//!     async fn on_app_message(&self, group: &str, msg: AppMessage) -> Result<(), CoreError> {
//!         self.ui.send(msg).await
//!     }
//!     // ... other methods
//! }
//!
//! // 2. Create a group (as steward)
//! let handle = create_group("my-chat", &identity_service)?;
//!
//! // 3. Send a message
//! let app_msg = ConversationMessage { message: b"Hello".to_vec(), .. }.into();
//! let packet = build_message(&handle, &identity_service, &app_msg).await?;
//! handler.on_outbound("my-chat", packet).await?;
//!
//! // 4. Process inbound messages (in your receive loop)
//! let result = process_inbound(&mut handle, &payload, subtopic, &mls, &identity).await?;
//! let action = dispatch_result::<DefaultProvider>(&handle, "my-chat", result, &consensus, &handler, &identity).await?;
//!
//! match action {
//!     DispatchAction::Done => { /* nothing more to do */ }
//!     DispatchAction::StartVoting(request) => { /* spawn voting task */ }
//!     DispatchAction::GroupUpdated => { /* refresh UI */ }
//!     DispatchAction::LeaveGroup => { /* cleanup group state */ }
//!     DispatchAction::JoinedGroup => { /* update state to Working */ }
//! }
//! ```
//!
//! # Module Organization
//!
//! - `api` - Core group operations (create, join, send, process)
//! - `consensus` - Voting workflow and dispatch
//! - `events` - [`GroupEventHandler`] trait
//! - `provider` - [`DeMlsProvider`] trait and [`DefaultProvider`]
//! - `types` - [`ProcessResult`], message conversions

mod api;
mod consensus;
mod error;
mod events;
mod group_handle;
mod group_update_handle;
mod provider;
mod types;

pub use api::{
    approved_proposals, approved_proposals_count, become_steward, build_key_package_message,
    build_message, create_batch_proposals, create_group, epoch_history, group_members,
    join_group_from_invite, prepare_to_join, process_inbound, resign_steward,
};
pub use consensus::{
    cast_vote, dispatch_result, forward_incoming_proposal, forward_incoming_vote,
    handle_consensus_event, request_steward_reelection, start_voting, DispatchAction,
};
pub use error::CoreError;
pub use events::GroupEventHandler;
pub use group_handle::GroupHandle;
pub use group_update_handle::{CurrentEpochProposals, ProposalId};
pub use provider::{DeMlsProvider, DefaultProvider};
pub use types::{
    convert_group_request_to_display, get_identity_from_group_update_request, message_types,
    MessageType, ProcessResult,
};
