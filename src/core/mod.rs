//! Reusable core for building DE-MLS chat applications.
//!
//! This module wraps MLS cryptography, consensus voting, and message routing.
//! Transport, UI, and state management live in the app layer.
//!
//! Integrators implement [`crate::core::GroupEventHandler`] (transport + UI
//! delivery) and feed inbound packets to [`crate::core::process_inbound`],
//! then dispatch the returned [`crate::core::ProcessResult`].
//! [`crate::core::DefaultProvider`] bundles in-memory backends for testing
//! and simple deployments.
//!
//! Submodules: `api` (group operations), `consensus` (pure consensus
//! application), `events` ([`crate::core::GroupEventHandler`]), `provider`
//! ([`crate::core::DeMlsProvider`] + [`crate::core::DefaultProvider`]).

mod api;
mod consensus;
mod error;
mod events;
mod group;
mod peer_scoring;
mod process_result;
mod proposal_kind;
mod provider;
mod steward_list;
mod steward_list_plugin;

// ── Core group operations ──
pub use api::{
    ElectionDecision, FreezeFinalizeResult, FreezeOutcome, build_create_proposal_request,
    build_key_package_message, create_commit_candidate, evaluate_election_initiation,
    finalize_freeze_round, group_members, is_deadlock_ecp_proposer, process_inbound,
};

// ── Consensus result application (pure, synchronous) ──
pub use consensus::{ConsensusApplyResult, apply_consensus_result};
pub use peer_scoring::{
    DEFAULT_THRESHOLD_PEER_SCORE, PeerScoreStorage, PeerScoringEvent, PeerScoringPlugin,
    PeerScoringService, ScoreEvent, ScoreOp, ScoreSnapshot, ScoringConfig, ScoringMemberDiff,
    ScoringProvider, emergency_score_ops, scoring_member_diff,
};

// ── Error type ──
pub use error::CoreError;

// ── Event handler trait and callback error ──
pub use events::{CallbackError, GroupEventHandler};

// ── Group state ──
pub(crate) use group::member_set;
pub use group::{
    DEFAULT_LIVENESS_CRITERIA_YES, DEFAULT_MAX_REELECTION_ATTEMPTS,
    DEFAULT_PENDING_UPDATE_MAX_EPOCHS, Group, PendingUpdate, ProposalId,
    auto_approved_leave_proposal_id, target_identity_of,
};

// ── Proposal classification ──
pub use proposal_kind::ProposalKind;

// ── Steward list ──
pub use steward_list::{ProtocolConfig, StewardList};
pub use steward_list_plugin::{
    DEFAULT_MAX_RETRIES, DeterministicStewardList, StewardListEvent, StewardListPlugin,
};

// ── Provider traits ──
pub use provider::{DeMlsProvider, DefaultProvider, ProviderConsensus};

// ── Process results ──
pub use process_result::ProcessResult;
