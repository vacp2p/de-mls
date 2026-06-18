//! Consensus integration: the plug-in contract, outcome application, and the
//! conversation-side opening/voting/bridge machinery.
//!
//! `plugin` defines the [`ConsensusPlugin`] trait and its associated types.
//! `dispatch` applies a resolved consensus outcome ([`apply_consensus_result`]).
//! `voting` submits and votes, `events` applies outcomes drained from the
//! event bus, and `bridge` holds the stateless consensus-library adapters
//! both use.

pub(crate) mod bridge;
mod dispatch;
mod events;
mod plugin;
mod voting;

pub use dispatch::{ConsensusApplyResult, apply_consensus_result};
pub use plugin::{ConsensusPlugin, ConsensusServiceFor, SyncConsensusReceiver};
pub use voting::CreatorVote;
