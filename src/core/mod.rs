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
mod state_machine;
mod steward_list_plugin;

// ── Core group operations ──
pub use api::{
    FreezeFinalizeResult, FreezeOutcome, build_create_proposal_request, build_key_package_message,
    compute_commit_hash, finalize_freeze_round, process_inbound,
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
pub use group::{
    BufferedCommitCandidate, DEFAULT_LIVENESS_CRITERIA_YES, DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
    Group, PendingUpdate, ProposalId, auto_approved_leave_proposal_id, member_set,
    target_identity_of,
};

// ── Proposal classification ──
pub use proposal_kind::ProposalKind;

// ── State machine (passive: state enum + named transitions) ──
pub use state_machine::{GroupState, GroupStateMachine};

// ── Steward list ──
pub use steward_list_plugin::{
    DEFAULT_MAX_RETRIES, DeterministicStewardList, ElectionDecision, StewardList,
    StewardListConfig, StewardListEvent, StewardListPlugin,
};

// ── Provider traits ──
pub use provider::{DeMlsProvider, DefaultProvider, ProviderConsensus};

// ── Process results ──
pub use process_result::ProcessResult;
