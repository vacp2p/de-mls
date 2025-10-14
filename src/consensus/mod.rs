//! Consensus module implementing HashGraph-like consensus for distributed voting
//!
//! This module implements the consensus protocol described in the [RFC](https://github.com/vacp2p/rfc-index/blob/consensus-hashgraph-like/vac/raw/consensus-hashgraphlike.md)
//!
//! The consensus is designed to work with GossipSub-like networks and provides:
//! - Proposal management
//! - Vote collection and validation
//! - Consensus reached detection

use crate::error::ConsensusError;
use crate::protos::messages::v1::consensus::v1::{Proposal, Vote};
use crate::LocalSigner;
use log::info;
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use uuid::Uuid;

pub mod service;

// Re-export protobuf types for compatibility with generated code
pub mod v1 {
    pub use crate::protos::messages::v1::consensus::v1::{Proposal, Vote};
}

pub use service::ConsensusService;

/// Consensus events emitted when consensus state changes
#[derive(Debug, Clone)]
pub enum ConsensusEvent {
    /// Consensus has been reached for a proposal
    ConsensusReached { proposal_id: u32, result: bool },
    /// Consensus failed due to timeout or other reasons
    ConsensusFailed { proposal_id: u32, reason: String },
}

/// Consensus configuration
#[derive(Debug, Clone)]
pub struct ConsensusConfig {
    /// Minimum number of votes required for consensus (as percentage of expected voters)
    pub consensus_threshold: f64,
    /// Timeout for consensus rounds in seconds
    pub consensus_timeout: u64,
    /// Maximum number of rounds before consensus is considered failed
    pub max_rounds: u32,
    /// Whether to use liveness criteria for silent peers
    pub liveness_criteria: bool,
}

impl Default for ConsensusConfig {
    fn default() -> Self {
        Self {
            consensus_threshold: 0.67, // 67% supermajority
            consensus_timeout: 20,     // 20 seconds
            max_rounds: 3,             // Maximum 3 rounds
            liveness_criteria: true,
        }
    }
}

/// Consensus state for a proposal
#[derive(Debug, Clone)]
pub enum ConsensusState {
    /// Proposal is active and accepting votes
    Active,
    /// Consensus has been reached
    ConsensusReached(bool), // true for yes, false for no
    /// Consensus failed (timeout or insufficient votes)
    Failed,
    /// Proposal has expired
    Expired,
}

/// Consensus session for a specific proposal
#[derive(Debug)]
pub struct ConsensusSession {
    pub proposal: Proposal,
    pub state: ConsensusState,
    pub votes: HashMap<Vec<u8>, Vote>, // vote_owner -> Vote
    pub created_at: u64,
    pub config: ConsensusConfig,
    pub event_sender: Option<broadcast::Sender<(String, ConsensusEvent)>>,
    pub group_name: String,
}

impl ConsensusSession {
    pub fn new(
        proposal: Proposal,
        config: ConsensusConfig,
        event_sender: Option<broadcast::Sender<(String, ConsensusEvent)>>,
        group_name: &str,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Failed to get current time")
            .as_secs();

        Self {
            proposal,
            state: ConsensusState::Active,
            votes: HashMap::new(),
            created_at: now,
            config,
            event_sender,
            group_name: group_name.to_string(),
        }
    }

    pub fn set_consensus_threshold(&mut self, consensus_threshold: f64) {
        self.config.consensus_threshold = consensus_threshold
    }

    /// Add a vote to the session
    pub fn add_vote(&mut self, vote: Vote) -> Result<(), ConsensusError> {
        match self.state {
            ConsensusState::Active => {
                // Check if voter already voted
                if self.votes.contains_key(&vote.vote_owner) {
                    return Err(ConsensusError::DuplicateVote);
                }

                // Add vote into the session and proposal
                self.votes.insert(vote.vote_owner.clone(), vote.clone());
                self.proposal.votes.push(vote.clone());

                // Check if consensus can be reached after adding the vote
                self.check_consensus();
                Ok(())
            }
            ConsensusState::ConsensusReached(_) => {
                info!(
                    "[consensus::mod::add_vote]: Consensus already reached for proposal {}, skipping vote",
                    self.proposal.proposal_id
                );
                Ok(())
            }
            _ => Err(ConsensusError::SessionNotActive),
        }
    }

    /// Count the number of required votes to reach consensus
    fn count_required_votes(&self) -> usize {
        let expected_voters = self.proposal.expected_voters_count as usize;
        if expected_voters <= 2 {
            expected_voters
        } else {
            ((expected_voters as f64) * self.config.consensus_threshold) as usize
        }
    }

    /// Check if consensus has been reached
    ///
    /// - `ConsensusReached(true)` if yes votes > no votes
    /// - `ConsensusReached(false)`
    ///     - if no votes > yes votes
    ///     - if no votes == yes votes and we have all votes
    /// - `Active`
    ///     - if no votes == yes votes and we don't have all votes
    ///     - if total votes < required votes (we wait for more votes)
    fn check_consensus(&mut self) {
        let total_votes = self.votes.len();
        let yes_votes = self.votes.values().filter(|v| v.vote).count();
        let no_votes = total_votes - yes_votes;

        // Check if we have all expected votes (only calculate consensus immediately if ALL votes received)
        let expected_voters = self.proposal.expected_voters_count as usize;
        let required_votes = self.count_required_votes();
        // For <= 2 voters, we require all votes to reach consensus
        if total_votes >= required_votes {
            // All votes received - calculate consensus immediately
            if yes_votes > no_votes {
                self.state = ConsensusState::ConsensusReached(true);
                info!(
                    "[consensus::mod::check_consensus]: Enough votes received {yes_votes}-{no_votes} - consensus reached: YES"
                );
                self.emit_consensus_event(ConsensusEvent::ConsensusReached {
                    proposal_id: self.proposal.proposal_id,
                    result: true,
                });
            } else if no_votes > yes_votes {
                self.state = ConsensusState::ConsensusReached(false);
                info!(
                    "[consensus::mod::check_consensus]: Enough votes received {yes_votes}-{no_votes} - consensus reached: NO"
                );
                self.emit_consensus_event(ConsensusEvent::ConsensusReached {
                    proposal_id: self.proposal.proposal_id,
                    result: false,
                });
            } else {
                // Tie - if it's all votes, we reject the proposal
                if total_votes == expected_voters {
                    self.state = ConsensusState::ConsensusReached(false);
                    info!(
                        "[consensus::mod::check_consensus]: All votes received and tie - consensus not reached"
                    );
                    self.emit_consensus_event(ConsensusEvent::ConsensusReached {
                        proposal_id: self.proposal.proposal_id,
                        result: false,
                    });
                } else {
                    // Tie - if it's not all votes, we wait for more votes
                    self.state = ConsensusState::Active;
                    info!(
                        "[consensus::mod::check_consensus]: Not enough votes received - consensus not reached"
                    );
                }
            }
        }
    }

    /// Emit a consensus event
    fn emit_consensus_event(&self, event: ConsensusEvent) {
        if let Some(sender) = &self.event_sender {
            info!(
                "[consensus::mod::emit_consensus_event]: Emitting consensus event: {event:?} for proposal {}",
                self.proposal.proposal_id
            );
            let _ = sender.send((self.group_name.clone(), event));
        }
    }

    /// Check if the session is still active
    pub fn is_active(&self) -> bool {
        matches!(self.state, ConsensusState::Active)
    }
}

/// Compute the hash of a vote
pub fn compute_vote_hash(vote: &Vote) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(vote.vote_id.to_le_bytes());
    hasher.update(&vote.vote_owner);
    hasher.update(vote.proposal_id.to_le_bytes());
    hasher.update(vote.timestamp.to_le_bytes());
    hasher.update([vote.vote as u8]);
    hasher.update(&vote.parent_hash);
    hasher.update(&vote.received_hash);
    hasher.finalize().to_vec()
}

/// Create a vote for an incoming proposal based on user's vote
async fn create_vote_for_proposal<S: LocalSigner>(
    proposal: &Proposal,
    user_vote: bool,
    signer: S,
) -> Result<Vote, ConsensusError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    // Get the latest vote as parent and received hash
    let (parent_hash, received_hash) = if let Some(latest_vote) = proposal.votes.last() {
        // Check if we already voted (same voter)
        let is_same_voter = latest_vote.vote_owner == signer.address_bytes();
        if is_same_voter {
            // Same voter: parent_hash should be the hash of our previous vote
            (latest_vote.vote_hash.clone(), Vec::new())
        } else {
            // Different voter: parent_hash is empty, received_hash is the hash of the latest vote
            (Vec::new(), latest_vote.vote_hash.clone())
        }
    } else {
        (Vec::new(), Vec::new())
    };

    // Create our vote with user's choice
    let mut vote = Vote {
        vote_id: Uuid::new_v4().as_u128() as u32,
        vote_owner: signer.address_bytes(),
        proposal_id: proposal.proposal_id,
        timestamp: now,
        vote: user_vote, // Use the user's actual vote choice
        parent_hash,
        received_hash,
        vote_hash: Vec::new(), // Will be computed below
        signature: Vec::new(), // Will be signed below
    };

    // Compute vote hash and signature
    vote.vote_hash = compute_vote_hash(&vote);
    let vote_bytes = vote.encode_to_vec();
    vote.signature = signer
        .local_sign_message(&vote_bytes)
        .await
        .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

    Ok(vote)
}

/// Statistics about consensus sessions
#[derive(Debug, Clone)]
pub struct ConsensusStats {
    pub total_sessions: usize,
    pub active_sessions: usize,
    pub consensus_reached: usize,
    pub failed_sessions: usize,
}

impl Default for ConsensusService {
    fn default() -> Self {
        Self::new()
    }
}
