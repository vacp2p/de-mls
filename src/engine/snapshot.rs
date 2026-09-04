//! Encoding engine state under the store keys and reading it back. The
//! write side runs in `finish`; the read side in `Engine::restore`.

use std::time::Duration;

use hashgraph_like_consensus::storage::ConsensusStorage;
use prost::Message;
use tracing::warn;

use crate::{
    ConversationError, ScoringConfig, StewardList, StewardListConfig,
    engine::{
        config::EngineConfig,
        handle::Engine,
        proposal_kind::ProposalKind,
        store::{Dirty, EngineStore, keys},
    },
    protos::de_mls::{
        engine::v1::{
            ApprovedProposal, ConsensusState, JoinEpoch, JoinEpochsState, MetaState,
            ProposalsState, ScoresState, SessionState, SkippedStewardsState, StewardListState,
        },
        messages::v1::{ConversationUpdateRequest, PeerScore, TimingConfig},
    },
};

impl<St: EngineStore> Engine<St> {
    /// Write every key the current driving call marked dirty, then clear
    /// the marks.
    pub(crate) fn flush(&mut self) -> Result<(), ConversationError> {
        if self.dirty.steward_list {
            self.write(keys::STEWARD_LIST, &self.steward_list_state())?;
        }
        if self.dirty.proposals {
            self.write(keys::PROPOSALS, &self.proposals_state())?;
        }
        if self.dirty.join_epochs {
            self.write(keys::JOIN_EPOCHS, &self.join_epochs_state())?;
        }
        if self.dirty.skipped_stewards {
            self.write(keys::SKIPPED_STEWARDS, &self.skipped_stewards_state())?;
        }
        if self.dirty.scores {
            self.write(keys::SCORES, &self.scores_state())?;
        }
        if self.dirty.consensus {
            let state = self.consensus_state()?;
            self.write(keys::CONSENSUS, &state)?;
        }
        if self.dirty.meta {
            self.write(keys::META, &self.meta_state())?;
        }
        self.dirty = Dirty::default();
        Ok(())
    }

    /// Encode and store one message under `key`.
    fn write(&mut self, key: &str, message: &impl Message) -> Result<(), ConversationError> {
        self.store
            .put(key, &message.encode_to_vec())
            .map_err(|e| ConversationError::Store(e.to_string()))
    }

    /// The bytes under `key`, or `None` if never written.
    pub(crate) fn read_key(store: &St, key: &str) -> Result<Option<Vec<u8>>, ConversationError> {
        store
            .get(key)
            .map_err(|e| ConversationError::Store(e.to_string()))
    }

    /// The config and the epoch it was snapshotted at, or `None` when
    /// `store` was never written.
    pub(crate) fn load_config(
        store: &St,
    ) -> Result<Option<(EngineConfig, u64)>, ConversationError> {
        let Some(bytes) = Self::read_key(store, keys::META)? else {
            return Ok(None);
        };
        let meta = MetaState::decode(bytes.as_slice())?;
        let mut config = EngineConfig::default();
        if let Some(timing) = &meta.timing {
            config.apply_timing(timing);
        }
        config.voting_delay = Duration::from_millis(meta.voting_delay_ms);
        config.liveness_criteria_yes = meta.liveness_criteria_yes;
        config.max_consensus_sessions = meta.max_consensus_sessions as usize;
        config.unanswered_sync_rounds = meta.unanswered_sync_rounds;
        config.steward_list = StewardListConfig::new(meta.sn_min as usize, meta.sn_max as usize)?;
        config.scoring = ScoringConfig {
            default_score: meta.default_peer_score,
        };
        Ok(Some((config, meta.snapshot_epoch)))
    }

    // ── encoders ─────────────────────────────────────────────────────

    fn steward_list_state(&self) -> StewardListState {
        let Some(list) = self.steward_list.current_list() else {
            return StewardListState::default();
        };
        StewardListState {
            members: list.members().to_vec(),
            election_epoch: list.election_epoch(),
            retry_round: list.retry_round(),
            sn_min: list.config().sn_min as u32,
            sn_max: list.config().sn_max as u32,
        }
    }

    fn proposals_state(&self) -> ProposalsState {
        let approved = self
            .queues
            .approved_proposals()
            .iter()
            .map(|(&proposal_id, request)| ApprovedProposal {
                proposal_id,
                request: Some(request.clone()),
            })
            .collect();
        ProposalsState {
            approved,
            urgent_commit_target: self
                .queues
                .urgent_commit_target()
                .map(<[u8]>::to_vec)
                .unwrap_or_default(),
        }
    }

    fn join_epochs_state(&self) -> JoinEpochsState {
        JoinEpochsState {
            entries: self
                .queues
                .member_join_epoch
                .iter()
                .map(|(member_id, &epoch)| JoinEpoch {
                    member_id: member_id.clone(),
                    epoch,
                })
                .collect(),
        }
    }

    fn skipped_stewards_state(&self) -> SkippedStewardsState {
        SkippedStewardsState {
            stewards: self.queues.skipped_stewards().iter().cloned().collect(),
        }
    }

    fn scores_state(&self) -> ScoresState {
        ScoresState {
            scores: self
                .scoring
                .all_members_with_scores()
                .into_iter()
                .map(|(member_id, score)| PeerScore { member_id, score })
                .collect(),
        }
    }

    fn consensus_state(&self) -> Result<ConsensusState, ConversationError> {
        let scope = self.conversation_id.clone();
        let sessions = self
            .consensus
            .storage()
            .list_scope_sessions(&scope)?
            .unwrap_or_default()
            .into_iter()
            .map(|session| {
                let mut proposal = session.proposal;
                proposal.votes = session.votes.into_values().collect();
                SessionState {
                    proposal: Some(proposal),
                    created_at: session.created_at,
                }
            })
            .collect();
        Ok(ConsensusState { sessions })
    }

    fn meta_state(&self) -> MetaState {
        MetaState {
            snapshot_epoch: self.epoch,
            timing: Some(TimingConfig::from(&self.config)),
            voting_delay_ms: self.config.voting_delay.as_millis() as u64,
            liveness_criteria_yes: self.config.liveness_criteria_yes,
            max_consensus_sessions: self.config.max_consensus_sessions as u64,
            unanswered_sync_rounds: self.config.unanswered_sync_rounds,
            sn_min: self.config.steward_list.sn_min as u32,
            sn_max: self.config.steward_list.sn_max as u32,
            default_peer_score: self.scoring.default_score(),
        }
    }

    // ── decoders ─────────────────────────────────────────────────────

    /// Install the steward list, or nothing when the message is empty (no
    /// list had been installed when the snapshot was written).
    pub(crate) fn restore_steward_list(&mut self, bytes: &[u8]) -> Result<(), ConversationError> {
        let state = StewardListState::decode(bytes)?;
        if state.members.is_empty() {
            return Ok(());
        }
        let config = StewardListConfig::new(state.sn_min as usize, state.sn_max as usize)?;
        self.steward_list.set_config(config.clone());
        let list = StewardList::from_parts(
            state.members,
            config,
            state.election_epoch,
            state.retry_round,
        );
        self.steward_list.restore_list(list);
        Ok(())
    }

    pub(crate) fn restore_proposals(&mut self, bytes: &[u8]) -> Result<(), ConversationError> {
        let state = ProposalsState::decode(bytes)?;
        for approved in state.approved {
            if let Some(request) = approved.request {
                self.queues
                    .insert_approved_proposal(approved.proposal_id, request);
            }
        }
        if !state.urgent_commit_target.is_empty() {
            self.queues
                .set_urgent_commit_target(state.urgent_commit_target);
        }
        Ok(())
    }

    pub(crate) fn restore_join_epochs(&mut self, bytes: &[u8]) -> Result<(), ConversationError> {
        let state = JoinEpochsState::decode(bytes)?;
        for entry in state.entries {
            self.queues
                .member_join_epoch
                .insert(entry.member_id, entry.epoch);
        }
        Ok(())
    }

    pub(crate) fn restore_skipped_stewards(
        &mut self,
        bytes: &[u8],
    ) -> Result<(), ConversationError> {
        let state = SkippedStewardsState::decode(bytes)?;
        for steward in state.stewards {
            self.queues.skip_steward(steward);
        }
        Ok(())
    }

    pub(crate) fn restore_scores(&mut self, bytes: &[u8]) -> Result<(), ConversationError> {
        let state = ScoresState::decode(bytes)?;
        let scores = state
            .scores
            .into_iter()
            .map(|s| (s.member_id, s.score))
            .collect();
        self.scoring.restore_scores(scores);
        Ok(())
    }

    /// Replay every persisted consensus session, lowest `ProposalKind`
    /// first so an emergency's partial freeze can never block the replay of
    /// a lower-priority session that was already open before the restart.
    /// A session that fails to replay is logged and dropped rather than
    /// failing the whole restore.
    pub(crate) fn restore_sessions(&mut self, bytes: &[u8]) -> Result<(), ConversationError> {
        let state = ConsensusState::decode(bytes)?;
        let mut sessions = state.sessions;
        sessions.sort_by_key(session_kind);
        for session in sessions {
            let Some(proposal) = session.proposal else {
                continue;
            };
            let proposal_id = proposal.proposal_id;
            if let Err(e) = self.replay_session(proposal, session.created_at) {
                warn!(
                    conversation = %self.conversation_id,
                    proposal_id,
                    error = %e,
                    "consensus session replay failed; dropped"
                );
            }
        }
        Ok(())
    }
}

/// The `ProposalKind` a persisted session's payload decodes to, `Commit`
/// (the lowest) for one that fails to decode.
fn session_kind(session: &SessionState) -> ProposalKind {
    session
        .proposal
        .as_ref()
        .and_then(|p| ConversationUpdateRequest::decode(p.payload.as_slice()).ok())
        .as_ref()
        .map(ProposalKind::of)
        .unwrap_or(ProposalKind::Commit)
}

#[cfg(test)]
mod tests {
    use hashgraph_like_consensus::protos::consensus::v1::Proposal;

    use super::*;
    use crate::{
        ScoreEvent, ScoreOp,
        engine::{
            store::InMemoryStore,
            types::{MemberId, Timestamp},
        },
        protos::de_mls::messages::v1::{MemberInvite, conversation_update_request},
    };

    fn member(id: u8) -> Vec<u8> {
        vec![id; 8]
    }

    fn invite_request(joiner: &[u8]) -> ConversationUpdateRequest {
        ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::MemberInvite(
                MemberInvite {
                    key_package_bytes: b"kp".to_vec(),
                    member_id: joiner.to_vec(),
                },
            )),
        }
    }

    /// A peer proposal as it arrives on the wire: opened by `owner`, still
    /// well inside its expiration window, no votes bundled.
    fn peer_proposal(
        owner: &[u8],
        proposal_id: u32,
        request: &ConversationUpdateRequest,
        expected_voters: u32,
    ) -> Proposal {
        Proposal {
            name: format!("p{proposal_id}"),
            payload: request.encode_to_vec(),
            proposal_id,
            proposal_owner: owner.to_vec(),
            votes: Vec::new(),
            expected_voters_count: expected_voters,
            round: 1,
            timestamp: 0,
            expiration_timestamp: 600,
            liveness_criteria_yes: true,
        }
    }

    /// Every key round-trips through `flush` and `Engine::restore`: the
    /// steward list, the approved queue and its urgent target, a join
    /// epoch, a skipped steward, a moved score, and an open consensus
    /// session (recomputed via `on_incoming_proposal`, matching the path
    /// `replay_session` also drives).
    #[test]
    fn each_key_round_trips() {
        let members: Vec<MemberId> = (1..=4).map(|i| MemberId::from(member(i))).collect();
        let (mut engine, _) = Engine::create(
            "conv",
            members[0].clone(),
            0,
            &members,
            EngineConfig::default(),
            InMemoryStore::default(),
        )
        .expect("create");
        engine.begin(Timestamp::ZERO);

        engine
            .queues
            .insert_approved_proposal(7, invite_request(&member(9)));
        engine.queues.set_urgent_commit_target(member(2));
        engine.queues.member_join_epoch.insert(member(9), 3);
        engine.queues.skip_steward(member(3));
        engine.apply_score_ops(&[ScoreOp {
            member_id: member(2),
            event: ScoreEvent::SuccessfulCommit,
        }]);

        let request = ConversationUpdateRequest::remove_member(member(4));
        let proposal = peer_proposal(&member(2), 11, &request, members.len() as u32);
        engine.on_incoming_proposal(&member(2), proposal).unwrap();

        // The state above was built by direct queue/scoring calls, not the
        // marked driving-call paths, so mark every key dirty before the
        // flush this test is exercising.
        engine.dirty = Dirty::ALL;
        engine.flush().expect("flush");

        let store = engine.store().clone();
        let (restored, _) =
            Engine::restore("conv", members[0].clone(), 0, &members, store).expect("restore");

        assert_eq!(
            restored
                .steward_list
                .current_list()
                .map(|l| l.members().to_vec()),
            engine
                .steward_list
                .current_list()
                .map(|l| l.members().to_vec()),
            "steward list members"
        );
        assert_eq!(
            restored.steward_list.election_epoch(),
            engine.steward_list.election_epoch(),
            "steward list election epoch"
        );
        assert_eq!(
            restored
                .queues
                .approved_proposals()
                .keys()
                .collect::<Vec<_>>(),
            engine
                .queues
                .approved_proposals()
                .keys()
                .collect::<Vec<_>>(),
            "approved ids in order"
        );
        assert_eq!(
            restored.queues.urgent_commit_target(),
            engine.queues.urgent_commit_target(),
            "urgent commit target"
        );
        assert_eq!(
            restored.queues.member_join_epoch.get(&member(9)),
            Some(&3),
            "join epoch"
        );
        assert!(
            restored.queues.skipped_stewards().contains(&member(3)),
            "skipped steward"
        );
        assert_eq!(
            restored.scoring.score_for(&member(2)),
            engine.scoring.score_for(&member(2)),
            "moved score"
        );
        assert!(restored.queues.is_voting(11), "session replayed as voting");
        assert!(
            restored.timing.pending_consensus_timeouts.contains_key(&11),
            "consensus timeout re-armed"
        );
    }
}
