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
//! Submodules: `freeze` (round selection + apply), `inbound` (app-subtopic
//! packet routing), `proposal_framing` (welcome-subtopic + consensus-library
//! framing helpers), `consensus` (pure consensus result application),
//! `events` ([`crate::core::GroupEventHandler`]), `provider`
//! ([`crate::core::DeMlsProvider`] + [`crate::core::DefaultProvider`]).

mod consensus;
mod error;
mod events;
mod freeze;
mod group;
mod inbound;
mod peer_scoring;
mod process_result;
mod proposal_framing;
mod proposal_kind;
mod provider;
mod steward_list;

// ── Core group operations ──
pub use freeze::{FreezeFinalizeResult, FreezeOutcome, compute_commit_hash, finalize_freeze_round};
pub use inbound::process_inbound;
pub use proposal_framing::{build_create_proposal_request, build_key_package_message};

// ── Per-conversation types: state, handle, state machine, config ──
pub use group::{
    BufferedCommitCandidate, DEFAULT_COMMIT_INACTIVITY_DURATION, DEFAULT_CONSENSUS_TIMEOUT,
    DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_LIVENESS_CRITERIA_YES, DEFAULT_PEER_SCORE,
    DEFAULT_PENDING_UPDATE_MAX_EPOCHS, DEFAULT_PROPOSAL_EXPIRATION,
    DEFAULT_RECOVERY_INACTIVITY_DURATION, DEFAULT_VOTING_DELAY, Group, GroupConfig, GroupHandle,
    GroupState, GroupStateMachine, OperatingMode, PendingUpdate, ProposalId, member_set,
    self_leave_proposal_id, target_identity_of,
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

// ── Proposal classification ──
pub use proposal_kind::ProposalKind;

// ── Steward list ──
pub use steward_list::{
    DEFAULT_MAX_RETRIES, DeterministicStewardList, ElectionDecision, StewardList,
    StewardListConfig, StewardListEvent, StewardListPlugin,
};

// ── Provider traits ──
pub use provider::{DeMlsProvider, DefaultProvider, ProviderConsensus};

// ── Process results ──
pub use process_result::ProcessResult;
