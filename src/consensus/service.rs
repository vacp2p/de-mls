//! Consensus service for managing consensus sessions and HashGraph integration
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, RwLock};
use tracing::info;
use uuid::Uuid;

use crate::consensus::{
    compute_vote_hash, create_vote_for_proposal, ConsensusConfig, ConsensusEvent, ConsensusSession,
    ConsensusState, ConsensusStats,
};
use crate::error::ConsensusError;
use crate::protos::messages::v1::consensus::v1::{Proposal, Vote};
use crate::{verify_vote_hash, LocalSigner};

/// Consensus service that manages multiple consensus sessions for multiple groups
#[derive(Clone)]
pub struct ConsensusService {
    /// Active consensus sessions organized by group: group_name -> proposal_id -> session
    sessions: Arc<RwLock<HashMap<String, HashMap<u32, ConsensusSession>>>>,
    /// Maximum number of voting sessions to keep per group
    max_sessions_per_group: usize,
    /// Event sender for consensus events
    event_sender: broadcast::Sender<(String, ConsensusEvent)>,
}

impl ConsensusService {
    /// Create a new consensus service
    pub fn new() -> Self {
        let (event_sender, _) = broadcast::channel(1000);
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            max_sessions_per_group: 10,
            event_sender,
        }
    }

    /// Create a new consensus service with custom max sessions per group
    pub fn new_with_max_sessions(max_sessions_per_group: usize) -> Self {
        let (event_sender, _) = broadcast::channel(1000);
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            max_sessions_per_group,
            event_sender,
        }
    }

    /// Subscribe to consensus events
    pub fn subscribe_to_events(&self) -> broadcast::Receiver<(String, ConsensusEvent)> {
        self.event_sender.subscribe()
    }

    pub async fn set_consensus_threshold_for_group_session(
        &mut self,
        group_name: &str,
        proposal_id: u32,
        consensus_threshold: f64,
    ) -> Result<(), ConsensusError> {
        let mut sessions = self.sessions.write().await;
        let group_sessions = sessions
            .entry(group_name.to_string())
            .or_insert_with(HashMap::new);

        let session = group_sessions
            .get_mut(&proposal_id)
            .ok_or(ConsensusError::SessionNotFound)?;

        session.set_consensus_threshold(consensus_threshold);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_proposal(
        &self,
        group_name: &str,
        name: String,
        payload: String,
        proposal_owner: Vec<u8>,
        expected_voters_count: u32,
        expiration_time: u64,
        liveness_criteria_yes: bool,
    ) -> Result<Proposal, ConsensusError> {
        let proposal_id = Uuid::new_v4().as_u128() as u32;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let config = ConsensusConfig::default();

        // Create proposal with steward's vote
        let proposal = Proposal {
            name,
            payload,
            proposal_id,
            proposal_owner,
            votes: vec![],
            expected_voters_count,
            round: 1,
            timestamp: now,
            expiration_time: now + expiration_time,
            liveness_criteria_yes,
        };

        // Create consensus session

        let session = ConsensusSession::new(
            proposal.clone(),
            config.clone(),
            Some(self.event_sender.clone()),
            group_name,
        );

        // Get timeout from session config before adding to sessions
        let timeout_seconds = config.consensus_timeout;

        // Add session to group and handle cleanup in a single lock operation
        {
            let mut sessions = self.sessions.write().await;
            let group_sessions = sessions
                .entry(group_name.to_string())
                .or_insert_with(HashMap::new);
            group_sessions.insert(proposal_id, session);

            // Clean up old sessions if we exceed the limit (within the same lock)
            if group_sessions.len() > self.max_sessions_per_group {
                // Sort sessions by creation time and keep the most recent ones
                let mut session_entries: Vec<_> = group_sessions.drain().collect();
                session_entries.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));

                // Keep only the most recent sessions
                for (proposal_id, session) in session_entries
                    .into_iter()
                    .take(self.max_sessions_per_group)
                {
                    group_sessions.insert(proposal_id, session);
                }
            }
        }

        // Start automatic timeout handling for this proposal using session config
        let self_clone = self.clone();
        let group_name_owned = group_name.to_string();
        tokio::spawn(async move {
            let timeout_duration = std::time::Duration::from_secs(timeout_seconds);
            tokio::time::sleep(timeout_duration).await;

            if self_clone
                .get_consensus_result(&group_name_owned, proposal_id)
                .await
                .is_some()
            {
                info!(
                    "Consensus result already exists for proposal {proposal_id}, skipping timeout"
                );
                return;
            }

            // Apply timeout consensus if still active
            if self_clone
                .handle_consensus_timeout(&group_name_owned, proposal_id)
                .await
                .is_ok()
            {
                info!(
                    "Automatic timeout applied for proposal {proposal_id} after {timeout_seconds}s"
                );
            }
        });

        Ok(proposal)
    }

    /// Create a new proposal with steward's vote attached
    pub async fn vote_on_proposal<S: LocalSigner>(
        &self,
        group_name: &str,
        proposal_id: u32,
        steward_vote: bool,
        signer: S,
    ) -> Result<Proposal, ConsensusError> {
        let vote_id = Uuid::new_v4().as_u128() as u32;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        // Create steward's vote first
        let steward_vote_obj = Vote {
            vote_id,
            vote_owner: signer.address_bytes(),
            proposal_id,
            timestamp: now,
            vote: steward_vote,
            parent_hash: Vec::new(),   // First vote, no parent
            received_hash: Vec::new(), // First vote, no received
            vote_hash: Vec::new(),     // Will be computed below
            signature: Vec::new(),     // Will be signed below
        };

        // Compute vote hash and signature for steward's vote
        let mut steward_vote_obj = steward_vote_obj;
        steward_vote_obj.vote_hash = compute_vote_hash(&steward_vote_obj);
        let vote_bytes = steward_vote_obj.encode_to_vec();
        steward_vote_obj.signature = signer
            .local_sign_message(&vote_bytes)
            .await
            .map_err(|e| ConsensusError::InvalidSignature(e.to_string()))?;

        let mut sessions = self.sessions.write().await;
        let group_sessions = sessions
            .entry(group_name.to_string())
            .or_insert_with(HashMap::new);
        let session = group_sessions
            .get_mut(&proposal_id)
            .ok_or(ConsensusError::SessionNotFound)?;

        session.add_vote(steward_vote_obj.clone())?;

        Ok(session.proposal.clone())
    }

    /// 1. Check the signatures of the each votes in proposal, in particular for proposal P_1,
    ///    verify the signature of V_1 where V_1 = P_1.votes\[0\] with V_1.signature and V_1.vote_owner
    /// 2. Do parent_hash check: If there are repeated votes from the same sender,
    ///    check that the hash of the former vote is equal to the parent_hash of the later vote.
    /// 3. Do received_hash check: If there are multiple votes in a proposal,
    ///    check that the hash of a vote is equal to the received_hash of the next one.
    pub fn validate_proposal(&self, proposal: &Proposal) -> Result<(), ConsensusError> {
        // Validate each vote individually first
        for vote in proposal.votes.iter() {
            self.validate_vote(vote, proposal.expiration_time)?;
        }

        // Validate vote chain integrity according to RFC
        self.validate_vote_chain(&proposal.votes)?;

        Ok(())
    }

    fn validate_vote(&self, vote: &Vote, expiration_time: u64) -> Result<(), ConsensusError> {
        if vote.vote_owner.is_empty() {
            return Err(ConsensusError::EmptyVoteOwner);
        }

        if vote.vote_hash.is_empty() {
            return Err(ConsensusError::EmptyVoteHash);
        }

        if vote.signature.is_empty() {
            return Err(ConsensusError::EmptySignature);
        }

        let expected_hash = compute_vote_hash(vote);
        if vote.vote_hash != expected_hash {
            return Err(ConsensusError::InvalidVoteHash);
        }

        // Encode vote without signature to verify signature
        let mut vote_copy = vote.clone();
        vote_copy.signature = Vec::new();
        let vote_copy_bytes = vote_copy.encode_to_vec();

        // Validate signature
        let verified = verify_vote_hash(&vote.signature, &vote.vote_owner, &vote_copy_bytes)?;

        if !verified {
            return Err(ConsensusError::InvalidVoteSignature);
        }

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        // Check that vote timestamp is not in the future
        if vote.timestamp > now {
            return Err(ConsensusError::InvalidVoteTimestamp);
        }

        // Check that vote timestamp is within expiration threshold
        if now - vote.timestamp > expiration_time {
            return Err(ConsensusError::VoteExpired);
        }

        Ok(())
    }

    /// Validate vote chain integrity according to RFC specification
    fn validate_vote_chain(&self, votes: &[Vote]) -> Result<(), ConsensusError> {
        if votes.len() <= 1 {
            return Ok(());
        }

        for i in 0..votes.len() - 1 {
            let current_vote = &votes[i];
            let next_vote = &votes[i + 1];

            // RFC requirement: received_hash of next vote should equal hash of current vote
            if current_vote.vote_hash != next_vote.received_hash {
                return Err(ConsensusError::ReceivedHashMismatch);
            }

            // RFC requirement: if same voter, parent_hash should equal hash of previous vote
            if current_vote.vote_owner == next_vote.vote_owner
                && current_vote.vote_hash != next_vote.parent_hash
            {
                return Err(ConsensusError::ParentHashMismatch);
            }
        }

        Ok(())
    }

    /// Process incoming proposal message
    pub async fn process_incoming_proposal(
        &self,
        group_name: &str,
        proposal: Proposal,
    ) -> Result<(), ConsensusError> {
        info!(
            "[consensus::service::process_incoming_proposal]: Processing incoming proposal for group {group_name}"
        );
        let mut sessions = self.sessions.write().await;
        let group_sessions = sessions
            .entry(group_name.to_string())
            .or_insert_with(HashMap::new);

        // Check if proposal already exists
        if group_sessions.contains_key(&proposal.proposal_id) {
            return Err(ConsensusError::ProposalAlreadyExist);
        }

        // Validate proposal including vote chain integrity
        self.validate_proposal(&proposal)?;

        // Create new session without our vote - user will vote later
        let mut session = ConsensusSession::new(
            proposal.clone(),
            ConsensusConfig::default(),
            Some(self.event_sender.clone()),
            group_name,
        );

        session.add_vote(proposal.votes[0].clone())?;
        group_sessions.insert(proposal.proposal_id, session);

        // Clean up old sessions if we exceed the limit (within the same lock)
        if group_sessions.len() > self.max_sessions_per_group {
            // Sort sessions by creation time and keep the most recent ones
            let mut session_entries: Vec<_> = group_sessions.drain().collect();
            session_entries.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));

            // Keep only the most recent sessions
            for (proposal_id, session) in session_entries
                .into_iter()
                .take(self.max_sessions_per_group)
            {
                group_sessions.insert(proposal_id, session);
            }
        }

        info!("[consensus::service::process_incoming_proposal]: Proposal stored, waiting for user vote");
        Ok(())
    }

    /// Process user vote for a proposal
    pub async fn process_user_vote<S: LocalSigner>(
        &self,
        group_name: &str,
        proposal_id: u32,
        user_vote: bool,
        signer: S,
    ) -> Result<Vote, ConsensusError> {
        let mut sessions = self.sessions.write().await;
        let group_sessions = sessions
            .get_mut(group_name)
            .ok_or(ConsensusError::GroupNotFound)?;

        let session = group_sessions
            .get_mut(&proposal_id)
            .ok_or(ConsensusError::SessionNotFound)?;

        // Check if user already voted
        let user_address = signer.address_bytes();
        if session.votes.values().any(|v| v.vote_owner == user_address) {
            return Err(ConsensusError::UserAlreadyVoted);
        }

        // Create our vote based on the user's choice
        let our_vote = create_vote_for_proposal(&session.proposal, user_vote, signer).await?;

        session.add_vote(our_vote.clone())?;

        Ok(our_vote)
    }

    /// Process incoming vote
    pub async fn process_incoming_vote(
        &self,
        group_name: &str,
        vote: Vote,
    ) -> Result<(), ConsensusError> {
        info!(
            "[consensus::service::process_incoming_vote]: Processing incoming vote for group {group_name}"
        );
        let mut sessions = self.sessions.write().await;
        let group_sessions = sessions
            .get_mut(group_name)
            .ok_or(ConsensusError::GroupNotFound)?;

        let session = group_sessions
            .get_mut(&vote.proposal_id)
            .ok_or(ConsensusError::SessionNotFound)?;

        self.validate_vote(&vote, session.proposal.expiration_time)?;

        // Add vote to session
        session.add_vote(vote.clone())?;

        Ok(())
    }

    /// Get liveness criteria for a proposal
    pub async fn get_proposal_liveness_criteria(
        &self,
        group_name: &str,
        proposal_id: u32,
    ) -> Option<bool> {
        let sessions = self.sessions.read().await;
        if let Some(group_sessions) = sessions.get(group_name) {
            if let Some(session) = group_sessions.get(&proposal_id) {
                return Some(session.proposal.liveness_criteria_yes);
            }
        }
        None
    }

    /// Get consensus result for a proposal
    pub async fn get_consensus_result(&self, group_name: &str, proposal_id: u32) -> Option<bool> {
        let sessions = self.sessions.read().await;
        if let Some(group_sessions) = sessions.get(group_name) {
            if let Some(session) = group_sessions.get(&proposal_id) {
                match session.state {
                    ConsensusState::ConsensusReached(result) => Some(result),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Get active proposals for a specific group
    pub async fn get_active_proposals(&self, group_name: &str) -> Vec<Proposal> {
        let sessions = self.sessions.read().await;
        if let Some(group_sessions) = sessions.get(group_name) {
            group_sessions
                .values()
                .filter(|session| session.is_active())
                .map(|session| session.proposal.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Clean up expired sessions for all groups
    pub async fn cleanup_expired_sessions(&self) {
        let mut sessions = self.sessions.write().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Failed to get current time")
            .as_secs();

        let group_names: Vec<String> = sessions.keys().cloned().collect();

        for group_name in group_names {
            if let Some(group_sessions) = sessions.get_mut(&group_name) {
                group_sessions.retain(|_, session| {
                    now <= session.proposal.expiration_time && session.is_active()
                });

                // Clean up old sessions if we exceed the limit
                if group_sessions.len() > self.max_sessions_per_group {
                    // Sort sessions by creation time and keep the most recent ones
                    let mut session_entries: Vec<_> = group_sessions.drain().collect();
                    session_entries.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));

                    // Keep only the most recent sessions
                    for (proposal_id, session) in session_entries
                        .into_iter()
                        .take(self.max_sessions_per_group)
                    {
                        group_sessions.insert(proposal_id, session);
                    }
                }
            }
        }
    }

    /// Get session statistics for a specific group
    pub async fn get_group_stats(&self, group_name: &str) -> ConsensusStats {
        let sessions = self.sessions.read().await;
        if let Some(group_sessions) = sessions.get(group_name) {
            let total_sessions = group_sessions.len();
            let active_sessions = group_sessions.values().filter(|s| s.is_active()).count();
            let consensus_reached = group_sessions
                .values()
                .filter(|s| matches!(s.state, ConsensusState::ConsensusReached(_)))
                .count();

            ConsensusStats {
                total_sessions,
                active_sessions,
                consensus_reached,
                failed_sessions: total_sessions - active_sessions - consensus_reached,
            }
        } else {
            ConsensusStats {
                total_sessions: 0,
                active_sessions: 0,
                consensus_reached: 0,
                failed_sessions: 0,
            }
        }
    }

    /// Get overall session statistics across all groups
    pub async fn get_overall_stats(&self) -> ConsensusStats {
        let sessions = self.sessions.read().await;
        let mut total_sessions = 0;
        let mut active_sessions = 0;
        let mut consensus_reached = 0;

        for group_sessions in sessions.values() {
            total_sessions += group_sessions.len();
            active_sessions += group_sessions.values().filter(|s| s.is_active()).count();
            consensus_reached += group_sessions
                .values()
                .filter(|s| matches!(s.state, ConsensusState::ConsensusReached(_)))
                .count();
        }

        ConsensusStats {
            total_sessions,
            active_sessions,
            consensus_reached,
            failed_sessions: total_sessions - active_sessions - consensus_reached,
        }
    }

    /// Get all group names that have active sessions
    pub async fn get_active_groups(&self) -> Vec<String> {
        let sessions = self.sessions.read().await;
        sessions
            .iter()
            .filter(|(_, group_sessions)| {
                group_sessions.values().any(|session| session.is_active())
            })
            .map(|(group_name, _)| group_name.clone())
            .collect()
    }

    /// Remove all sessions for a specific group
    pub async fn remove_group_sessions(&self, group_name: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.remove(group_name);
    }

    /// Check if we have enough votes for consensus (2n/3 threshold)
    pub async fn has_sufficient_votes(&self, group_name: &str, proposal_id: u32) -> bool {
        let sessions = self.sessions.read().await;

        if let Some(group_sessions) = sessions.get(group_name) {
            if let Some(session) = group_sessions.get(&proposal_id) {
                let total_votes = session.votes.len() as u32;
                let expected_voters = session.proposal.expected_voters_count;
                self.check_sufficient_votes(
                    total_votes,
                    expected_voters,
                    session.config.consensus_threshold,
                )
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Handle consensus when timeout is reached
    pub async fn handle_consensus_timeout(
        &self,
        group_name: &str,
        proposal_id: u32,
    ) -> Result<bool, ConsensusError> {
        // First, check if consensus was already reached to avoid unnecessary work
        let mut sessions = self.sessions.write().await;
        if let Some(group_sessions) = sessions.get_mut(group_name) {
            if let Some(session) = group_sessions.get_mut(&proposal_id) {
                // Check if consensus was already reached
                match session.state {
                    crate::consensus::ConsensusState::ConsensusReached(result) => {
                        info!("Consensus already reached for proposal {proposal_id}, skipping timeout");
                        Ok(result)
                    }
                    _ => {
                        // Calculate consensus result
                        let total_votes = session.votes.len() as u32;
                        let expected_voters = session.proposal.expected_voters_count;
                        let result = if self.check_sufficient_votes(
                            total_votes,
                            expected_voters,
                            session.config.consensus_threshold,
                        ) {
                            // We have sufficient votes (2n/3) - calculate result based on votes
                            self.calculate_consensus_result(
                                &session.votes,
                                session.proposal.liveness_criteria_yes,
                            )
                        } else {
                            // Insufficient votes - apply liveness criteria
                            session.proposal.liveness_criteria_yes
                        };

                        // Apply timeout consensus
                        session.state = crate::consensus::ConsensusState::ConsensusReached(result);
                        info!("Timeout consensus applied for proposal {proposal_id}: {result} (liveness criteria)");

                        // Emit consensus event
                        session.emit_consensus_event(
                            crate::consensus::ConsensusEvent::ConsensusReached {
                                proposal_id,
                                result,
                            },
                        );

                        Ok(result)
                    }
                }
            } else {
                Err(ConsensusError::SessionNotFound)
            }
        } else {
            Err(ConsensusError::SessionNotFound)
        }
    }

    /// Helper method to calculate required votes for consensus
    fn calculate_required_votes(&self, expected_voters: u32, consensus_threshold: f64) -> u32 {
        if expected_voters == 1 || expected_voters == 2 {
            expected_voters
        } else {
            ((expected_voters as f64) * consensus_threshold) as u32
        }
    }

    /// Helper method to check if sufficient votes exist for consensus
    fn check_sufficient_votes(
        &self,
        total_votes: u32,
        expected_voters: u32,
        consensus_threshold: f64,
    ) -> bool {
        let required_votes = self.calculate_required_votes(expected_voters, consensus_threshold);
        println!(
            "[consensus::service::check_sufficient_votes]: Total votes: {total_votes}, Expected voters: {expected_voters}, Consensus threshold: {consensus_threshold}, Required votes: {required_votes}"
        );
        total_votes >= required_votes
    }

    /// Helper method to calculate consensus result based on votes
    fn calculate_consensus_result(
        &self,
        votes: &HashMap<Vec<u8>, Vote>,
        liveness_criteria_yes: bool,
    ) -> bool {
        let total_votes = votes.len() as u32;
        let yes_votes = votes.values().filter(|v| v.vote).count() as u32;
        let no_votes = total_votes - yes_votes;

        if yes_votes > no_votes {
            true
        } else if no_votes > yes_votes {
            false
        } else {
            // Tie - apply liveness criteria
            liveness_criteria_yes
        }
    }
}
