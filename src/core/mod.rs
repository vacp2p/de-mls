//! Reusable core for building DE-MLS chat applications.
//!
//! Wraps MLS cryptography, consensus voting, and message routing. Transport,
//! UI, and state management live in the app layer.
//!
//! Integrators implement [`crate::core::ConversationEventHandler`] (transport
//! + UI delivery) and feed inbound packets to [`crate::core::process_inbound`],
//! then dispatch the returned [`crate::core::ProcessResult`].
//! [`crate::core::DefaultProvider`] bundles in-memory backends for tests
//! and simple deployments.
//!
//! Submodules: `conversation` (per-conversation state, handle, state machine,
//! config), `consensus` (pure consensus result application), `events`
//! ([`crate::core::ConversationEventHandler`]), `freeze` (round selection
//! + apply), `inbound` (app-subtopic packet routing), `peer_scoring`
//! (scoring plug-in contract), `process_result`
//! ([`crate::core::ProcessResult`]), `proposal_framing` (welcome-subtopic
//! + consensus-library framing helpers), `proposal_kind`
//! ([`crate::core::ProposalKind`] classifier), `provider`
//! ([`crate::core::DeMlsProvider`] + [`crate::core::DefaultProvider`]),
//! `steward_list` (steward-list plug-in).

mod consensus;
mod conversation;
mod error;
mod events;
mod freeze;
mod inbound;
mod peer_scoring;
mod process_result;
mod proposal_framing;
mod proposal_kind;
mod provider;
mod steward_list;

// ── Core conversation operations ──
pub use freeze::{FreezeFinalizeResult, FreezeOutcome, compute_commit_hash, finalize_freeze_round};
pub use inbound::process_inbound;
pub use proposal_framing::{build_create_proposal_request, build_key_package_message};

// ── Per-conversation types: state, handle, state machine, config ──
pub use conversation::{
    BufferedCommitCandidate, Conversation, ConversationConfig, ConversationHandle,
    ConversationState, ConversationStateMachine, DEFAULT_COMMIT_INACTIVITY_DURATION,
    DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_LIVENESS_CRITERIA_YES,
    DEFAULT_PENDING_UPDATE_MAX_EPOCHS, DEFAULT_PROPOSAL_EXPIRATION,
    DEFAULT_RECOVERY_INACTIVITY_DURATION, DEFAULT_VOTING_DELAY, OperatingMode, PendingUpdate,
    ProposalId, member_set, self_leave_proposal_id, target_identity_of,
};

// ── Consensus result application (pure, synchronous) ──
pub use consensus::{ConsensusApplyResult, apply_consensus_result};
pub use peer_scoring::{
    DEFAULT_PEER_SCORE, DEFAULT_THRESHOLD_PEER_SCORE, PeerScoreStorage, PeerScoringEvent,
    PeerScoringPlugin, PeerScoringService, ScoreEvent, ScoreOp, ScoreSnapshot, ScoringConfig,
    ScoringMemberDiff, ScoringProvider, emergency_score_ops, scoring_member_diff,
};

// ── Error type ──
pub use error::CoreError;

// ── Event handler trait and callback error ──
pub use events::{CallbackError, ConversationEventHandler};

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
