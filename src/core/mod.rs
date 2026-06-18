//! Reusable core for building DE-MLS chat applications.
//!
//! Wraps MLS cryptography, consensus voting, and message routing. Transport,
//! UI, and state management live in the app layer.
//!
//! Integrators drain `ConversationEvent` from each conversation (transport
//! + UI delivery) and feed inbound packets to [`crate::core::process_inbound`],
//! then dispatch the returned [`crate::core::ProcessResult`].
//! [`crate::defaults::DefaultConsensusPlugin`] bundles in-memory backends for tests
//! and simple deployments.
//!
//! Submodules: `conversation` (per-conversation state, queues, state machine,
//! config), `consensus` (pure consensus result application),
//! `consensus_plugin` ([`crate::core::ConsensusPlugin`] +
//! [`crate::defaults::DefaultConsensusPlugin`]), `conversation_plugins`
//! ([`crate::core::ConversationPluginsFactory`] — the per-conversation
//! plug-in factory bundle), `events` (`ConversationEvent`),
//! `freeze` (round selection + apply),
//! `inbound` (app-subtopic packet routing), `peer_scoring`
//! (scoring plug-in contract), `process_result`
//! ([`crate::core::ProcessResult`]), `proposal_kind`
//! ([`crate::core::ProposalKind`] classifier), `steward_list`
//! (steward-list plug-in).

mod consensus;
mod consensus_plugin;
mod conversation;
mod conversation_plugins;
mod error;
mod events;
mod freeze;
mod inbound;
mod peer_scoring;
mod process_result;
mod proposal_kind;
mod steward_list;

// ── Core conversation operations ──
pub use freeze::{
    CommitHash, FreezeFinalizeResult, FreezeOutcome, compute_commit_hash, finalize_freeze_round,
    replay_early_candidates,
};
pub use inbound::process_inbound;

// ── Per-conversation types: state, queues, state machine, config ──
pub use conversation::{
    BufferedCommitCandidate, ConversationConfig, ConversationQueues, ConversationState,
    ConversationStateMachine, DEFAULT_COMMIT_INACTIVITY_DURATION, DEFAULT_CONSENSUS_TIMEOUT,
    DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_LIVENESS_CRITERIA_YES,
    DEFAULT_PENDING_UPDATE_MAX_EPOCHS, DEFAULT_PROPOSAL_EXPIRATION,
    DEFAULT_RECOVERY_INACTIVITY_DURATION, DEFAULT_VOTING_DELAY, FreezeBufferOutcome, OperatingMode,
    ProposalId, member_set, self_leave_proposal_id, target_member_id_of,
};

// ── Consensus result application (pure, synchronous) ──
pub use consensus::{ConsensusApplyResult, apply_consensus_result};
pub use peer_scoring::{
    DEFAULT_PEER_SCORE, DEFAULT_THRESHOLD_PEER_SCORE, PeerScoreStorage, PeerScoringPlugin,
    PeerScoringService, ScoreEvent, ScoreOp, ScoreSnapshot, ScoringConfig, ScoringMemberDiff,
    default_score_deltas, emergency_score_ops, scoring_member_diff,
};

// ── Error type ──
pub use error::CoreError;

// ── Conversation-event types ──
pub use events::ConversationEvent;

// ── Proposal classification ──
pub use proposal_kind::ProposalKind;

// ── Steward list ──
pub use steward_list::{
    DEFAULT_MAX_RETRIES, DeterministicStewardList, ElectionDecision, StewardList,
    StewardListConfig, StewardListPlugin,
};

// ── Consensus plug-in trait ──
pub use consensus_plugin::{ConsensusPlugin, ConsensusServiceFor, SyncConsensusReceiver};

// ── Per-conversation plug-in bundle ──
pub use conversation_plugins::ConversationPluginsFactory;

// ── Process results ──
pub use process_result::{NoopReason, ProcessResult};
