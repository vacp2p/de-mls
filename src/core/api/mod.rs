//! Core API for MLS group operations.
//!
//! This module provides the fundamental building blocks for MLS group management.
//! All functions operate on [`Group`] instances for app-level state and
//! [`MlsService`] for MLS cryptographic operations.
//!
//! # Overview
//!
//! The API is organized into three categories:
//!
//! ## Group Lifecycle
//! - [`create_group`] - Create a new group as steward
//! - [`prepare_to_join`] - Prepare a handle for joining
//! - [`join_group_from_invite`] - Complete join with welcome message
//!
//! ## Message Operations
//! - [`build_message`] - Encrypt an application message for the group
//! - [`build_key_package_message`] - Create key package for joining
//!
//! ## Inbound Processing
//! - [`process_inbound`] - Process received packets, returns [`ProcessResult`]
//!
//! ## Steward Operations
//! - [`create_commit_candidate`] - Build/broadcast commit candidate (no immediate merge)
//! - [`finalize_freeze_round`] - Select and apply a buffered candidate

use openmls_rust_crypto::MemoryStorage;
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::core::{
    ProposalId,
    error::CoreError,
    group::{BufferedCommitCandidate, Group},
    process_result::ProcessResult,
    process_result::invitation_from_bytes,
};
use crate::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use crate::mls_crypto::{
    CommitCandidate as MlsCommitCandidate, DeMlsStorage, DecryptResult, GroupUpdate,
    KeyPackageBytes, MlsMessageKind, MlsProposalAction, MlsService, StagedCommitResult,
    key_package_bytes_from_json,
};
use crate::protos::de_mls::messages::v1::{
    AppMessage, CommitCandidate, GroupUpdateRequest, InviteMember, UserKeyPackage,
    ViolationEvidence, WelcomeMessage, app_message, group_update_request, welcome_message,
};

mod election;
mod freeze;
mod inbound;
mod lifecycle;
mod steward;

#[cfg(test)]
mod tests;

pub use election::{apply_election_result, validate_election_proposal};
pub use freeze::{FreezeFinalizeResult, finalize_freeze_round};
pub use inbound::process_inbound;
pub use lifecycle::{
    build_key_package_message, build_message, create_group, join_group_from_invite, prepare_to_join,
};
pub use steward::{create_commit_candidate, group_members};
