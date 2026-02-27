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
//! │  │  • Consensus integration                            │   │
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
//! 1. **`GroupEventHandler`** - Required trait for handling output events:
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
//! - **Consensus Integration** - Proposal creation, vote casting, result handling
//! - **Message Types** - Protobuf definitions for all protocol messages
//! - **Default Services** - `DefaultProvider` bundles standard implementations
//!
//! # Core Concepts
//!
//! ## GroupHandle
//!
//! `GroupHandle` is the per-group state container. It holds:
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
//! ## ProcessResult
//!
//! When processing inbound messages:
//! 1. Call `process_inbound()` → returns `ProcessResult`
//! 2. Match the result directly in your application layer
//!
//! # Quick Start
//!
//! ```ignore
//! use de_mls::core::{
//!     create_group, build_message, process_inbound,
//!     GroupEventHandler, GroupHandle, ProcessResult,
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
//! let result = process_inbound(&mut handle, &payload, subtopic, &mls).await?;
//! match result {
//!     ProcessResult::AppMessage(msg) => { handler.on_app_message("my-chat", msg).await?; }
//!     ProcessResult::GroupUpdated => { /* refresh UI */ }
//!     ProcessResult::LeaveGroup => { /* cleanup group state */ }
//!     ProcessResult::JoinedGroup(name) => { /* update state to Working */ }
//!     ProcessResult::GetUpdateRequest(req) => { /* start consensus vote */ }
//!     ProcessResult::Proposal(p) => { /* forward to consensus */ }
//!     ProcessResult::Vote(v) => { /* forward to consensus */ }
//!     ProcessResult::ViolationDetected(ev) => { /* start emergency vote */ }
//!     ProcessResult::Noop => {}
//! }
//! ```
//!
//! # Module Organization
//!
//! - `api` - Core group operations (create, join, send, process)
//! - `consensus` - Voting workflow and consensus result application
//! - `events` - `GroupEventHandler` trait
//! - `provider` - `DeMlsProvider` trait and `DefaultProvider`
//! - `types` - `ProcessResult`, message conversions

mod api;
mod consensus;
mod error;
mod events;
mod group_handle;
mod group_update_handle;
mod proposal_priority;
mod provider;
mod types;

// ── Core group operations ──
pub use api::{
    FreezeFinalizeResult, approved_proposals, approved_proposals_count, build_key_package_message,
    build_message, create_commit_candidate, create_group, epoch_history, finalize_freeze_round,
    group_members, join_group_from_invite, prepare_to_join, process_inbound,
};

// ── Consensus result application (pure, synchronous) ──
pub use consensus::{ConsensusOutcome, apply_consensus_result};

// ── Error type ──
pub use error::CoreError;

// ── Event handler trait ──
pub use events::GroupEventHandler;

// ── Group state ──
pub use group_handle::GroupHandle;

// ── Proposal types ──
pub use group_update_handle::ProposalId;

// ── Proposal priority ──
pub use proposal_priority::ProposalPriority;

// ── Provider traits ──
pub use provider::{DeMlsProvider, DefaultProvider};

// ── Core types ──
pub use types::ProcessResult;
