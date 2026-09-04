//! Opening proposals, casting votes, and the adapters that talk to the
//! consensus service.
//!
//! Everything runs inline on the driving call: opening a proposal starts the
//! consensus session right away, while the time-based follow-ups (auto-votes,
//! consensus timeouts) wait as deadlines that [`Engine::tick_deadlines`] fires
//! on a later tick. Wire messages leave as `Outbound::Control` bytes the
//! router seals; nothing here touches a group.

use std::time::Duration;

use hashgraph_like_consensus::{
    error::ConsensusError,
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    storage::ConsensusStorage,
    types::CreateProposalRequest,
    utils::build_vote,
};
use prost::Message;
use tracing::{debug, info, warn};

use crate::{
    ConversationError, ConversationState, CreatorVote,
    engine::{
        handle::Engine,
        store::EngineStore,
        types::{Decision, Event, MemberId, Output, Timestamp},
        util::{in_flight_target, self_leave_proposal_id},
    },
    proposal_kind::ProposalKind,
    protos::de_mls::messages::v1::{
        ControlMessage, ConversationUpdateRequest, control_message, conversation_update_request,
    },
};

/// Per-proposal consensus-session parameters.
struct ProposalParams {
    expected_voters: u32,
    proposal_expiration: Duration,
    consensus_timeout: Duration,
    liveness_criteria_yes: bool,
}

/// Encode `proposal` as the control message peers receive.
fn control_proposal(proposal: Proposal) -> Vec<u8> {
    ControlMessage {
        payload: Some(control_message::Payload::Proposal(proposal)),
    }
    .encode_to_vec()
}

/// Encode `vote` as the control message peers receive.
fn control_vote(vote: Vote) -> Vec<u8> {
    ControlMessage {
        payload: Some(control_message::Payload::Vote(vote)),
    }
    .encode_to_vec()
}

/// Fast-path proposals (`expected_voters_count == 1`) bypass peer voting, so
/// they are restricted to self-removal. Requiring the MLS-authenticated
/// `sender` to be both the owner and the `RemoveMember` target closes the
/// unilateral-removal vector an otherwise-free `expected_voters == 1` opens.
///
/// `true` when the proposal is allowed. A mismatch — different target, wrong
/// payload variant, or undecodable — is `false`.
fn authorize_fast_path_proposal(proposal: &Proposal, sender: &[u8]) -> bool {
    if proposal.expected_voters_count != 1 {
        return true;
    }
    if proposal.proposal_owner != sender {
        return false;
    }
    let Ok(request) = ConversationUpdateRequest::decode(proposal.payload.as_slice()) else {
        return false;
    };
    matches!(
        request.payload,
        Some(conversation_update_request::Payload::RemoveMember(ref r)) if r.member_id == sender
    )
}

impl<St: EngineStore> Engine<St> {
    // ── opening a round ──────────────────────────────────────────────

    /// Open a consensus vote for `request`; [`CreatorVote`] picks the wire
    /// shape and whether the engine asks for a vote of its own.
    ///
    /// Errors when the state machine forbids new proposals (freeze phases,
    /// partial freeze during an active emergency). No-ops when a change for
    /// the request's target member is already in flight locally. On success
    /// the proposal is queued as `Outbound::Control` and a consensus-timeout
    /// deadline is armed for [`Self::tick_deadlines`] to fire.
    ///
    /// An invite the engine opens itself carries no key-package decision: the
    /// router validated the key package before calling.
    pub(crate) fn initiate_proposal(
        &mut self,
        request: ConversationUpdateRequest,
        creator_vote: CreatorVote,
    ) -> Result<(), ConversationError> {
        // One in-flight change per member (local dedup): don't open a second
        // session for a target already being voted on or approved here. A
        // cross-node duplicate — two members proposing the same target — is
        // caught later at approval by `has_approved_*`.
        if let Some(target) = in_flight_target(&request)
            && self.queues.active_proposal_targets().contains(&target)
        {
            info!(
                conversation = %self.conversation_id,
                member = ?target,
                "proposal skipped: a change for this member is already in flight"
            );
            return Ok(());
        }

        let kind = ProposalKind::of(&request);
        let expected_voters = self.check_proposal_allowed(kind)?;

        let liveness_criteria_yes = self.config.liveness_criteria_yes;
        let consensus_timeout = self.config.consensus_timeout;
        let voting_delay = self.config.voting_delay;

        let (proposal_id, unbundled) = self.submit_proposal(
            &request,
            ProposalParams {
                expected_voters,
                proposal_expiration: self.config.proposal_expiration,
                consensus_timeout,
                liveness_criteria_yes,
            },
        )?;

        self.queues.track_voting_proposal(proposal_id, &request);
        if kind.is_emergency() {
            self.queues.insert_emergency(proposal_id);
        }
        // Removed again by `handle_consensus_outcome` if an outcome lands
        // before the deadline fires.
        self.register_consensus_timeout(proposal_id, consensus_timeout);

        match creator_vote {
            CreatorVote::Yes => {
                // Owner-bundling API, not the vote helper: peers don't have
                // the proposal yet, so a Vote-only message would be
                // undeliverable.
                let scope = self.conversation_id.clone();
                let now = self.now.as_secs();
                let proposal =
                    self.consensus
                        .cast_vote_and_get_proposal(&scope, proposal_id, true, now)?;
                info!(
                    conversation = %self.conversation_id,
                    proposal_id,
                    actor = "owner",
                    "YES vote cast (bundled at submit)"
                );
                self.send_control(control_proposal(proposal));
            }
            CreatorVote::Deferred => {
                self.send_control(control_proposal(unbundled));
                self.emit(Event::VoteRequested {
                    proposal_id,
                    request,
                });
                self.register_auto_vote(proposal_id, voting_delay, liveness_criteria_yes);
            }
        }
        // store: consensus sessions (WP3g)

        Ok(())
    }

    /// Open a self-leave round: `RemoveMember(self)` with one expected voter
    /// and the leaver's YES bundled, so it resolves synchronously and lands
    /// in the approved queue for the next steward commit.
    ///
    /// Safe to repeat: the pending-leave check catches local duplicates, and
    /// the deterministic [`self_leave_proposal_id`] dedupes retransmits
    /// inside the consensus library.
    pub(crate) fn initiate_self_leave(&mut self) -> Result<(), ConversationError> {
        if self.queues.is_pending_self_leave(&self.own) {
            info!(
                conversation = %self.conversation_id,
                "self-leave already in flight, ignoring duplicate"
            );
            return Ok(());
        }

        let request = ConversationUpdateRequest::remove_member(self.own.clone());
        let proposal_id = self_leave_proposal_id(&self.own);

        // Track before the session opens: the bundled YES fires the outcome
        // synchronously.
        self.queues.track_voting_proposal(proposal_id, &request);

        let submitted = self.submit_self_leave_proposal(ProposalParams {
            expected_voters: 1,
            proposal_expiration: self.config.proposal_expiration,
            consensus_timeout: self.config.consensus_timeout,
            liveness_criteria_yes: true,
        })?;

        // `None`: an earlier submit is already driving this proposal id; our
        // voting entry resolves on that session.
        let Some(proposal) = submitted else {
            return Ok(());
        };

        self.send_control(control_proposal(proposal));
        // store: consensus sessions (WP3g)
        Ok(())
    }

    // ── voting ───────────────────────────────────────────────────────

    /// Cast this member's vote in the local session and queue the Vote
    /// control message. The manual path and the auto-vote timer share it;
    /// the consensus library can't tell them apart. A pending auto-vote for
    /// the same proposal is dropped, so a manual choice wins.
    pub(crate) fn cast_local_vote(
        &mut self,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), ConversationError> {
        self.cancel_auto_vote(proposal_id);
        let scope = self.conversation_id.clone();
        let choice = if vote { "YES" } else { "NO" };
        info!(
            conversation = %self.conversation_id,
            proposal_id, choice, "vote cast"
        );
        let now = self.now.as_secs();
        let vote_msg = self.consensus.cast_vote(&scope, proposal_id, vote, now)?;
        self.send_control(control_vote(vote_msg));
        // store: consensus sessions (WP3g)
        Ok(())
    }

    /// Feed a peer's vote into the local consensus session. The vote owner
    /// must be the MLS-authenticated `sender`; anything else is forged and
    /// dropped.
    ///
    /// Late arrivals are swallowed (logged, `Ok`) so inbound dispatch keeps
    /// draining.
    pub(crate) fn forward_incoming_vote(
        &mut self,
        sender: &[u8],
        vote: Vote,
    ) -> Result<(), ConversationError> {
        let proposal_id = vote.proposal_id;
        if vote.vote_owner != sender {
            warn!(
                conversation = %self.conversation_id,
                proposal_id,
                owner = ?vote.vote_owner,
                sender = ?sender,
                "vote dropped: owner is not the authenticated sender"
            );
            return Ok(());
        }
        let outcome_applied_locally = self.queues.is_consensus_outcome_applied(proposal_id);
        let scope = self.conversation_id.clone();
        let now = self.now.as_secs();
        match self.consensus.process_incoming_vote(&scope, vote, now) {
            Ok(()) => {
                // store: consensus sessions (WP3g)
                Ok(())
            }
            Err(ConsensusError::SessionNotActive) => {
                debug!(
                    conversation = %self.conversation_id,
                    proposal_id,
                    "late vote dropped: consensus session already resolved"
                );
                Ok(())
            }
            Err(ConsensusError::SessionNotFound) => {
                if outcome_applied_locally {
                    debug!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "late vote dropped: session trimmed after local resolution"
                    );
                } else {
                    warn!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "vote for unknown proposal id dropped: no local session and not in resolved cache"
                    );
                }
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    // ── inbound proposals ────────────────────────────────────────────

    /// A peer's proposal arrived from the MLS-authenticated `sender`.
    ///
    /// Before forwarding to consensus, intent is mirrored into local
    /// buffers: emergency proposals set the partial-freeze flag, membership
    /// changes are buffered so a future epoch steward can retry them.
    ///
    /// RFC §"Partial Freeze Semantics": while an emergency proposal is
    /// unfinalized, lower-priority proposals from peers MUST be dropped —
    /// not buffered, forwarded to consensus, or surfaced for a vote — so
    /// they cannot land before the emergency resolves. The emergency itself
    /// (and any higher-priority proposal) is never blocked.
    ///
    /// An invite proposal does not reach a vote until the router has
    /// validated its key package: the engine asks for
    /// [`Decision::ValidateKeyPackage`] and waits for
    /// [`Self::key_package_checked`].
    pub(crate) fn on_incoming_proposal(
        &mut self,
        sender: &[u8],
        proposal: Proposal,
    ) -> Result<(), ConversationError> {
        if proposal.proposal_owner != sender {
            warn!(
                conversation = %self.conversation_id,
                proposal_id = proposal.proposal_id,
                owner = ?proposal.proposal_owner,
                sender = ?sender,
                "proposal dropped: owner is not the authenticated sender"
            );
            return Ok(());
        }
        if !authorize_fast_path_proposal(&proposal, sender) {
            warn!(
                conversation = %self.conversation_id,
                proposal_id = proposal.proposal_id,
                sender = ?sender,
                "single-voter proposal dropped: only self-removal may bypass peer voting"
            );
            return Ok(());
        }

        let decoded = match ConversationUpdateRequest::decode(proposal.payload.as_slice()) {
            Ok(req) => Some(req),
            Err(e) => {
                debug!(
                    proposal_id = proposal.proposal_id,
                    error = %e,
                    "incoming proposal payload failed to decode; treated as opaque commit"
                );
                None
            }
        };
        let kind = decoded
            .as_ref()
            .map(ProposalKind::of)
            .unwrap_or(ProposalKind::Commit);

        if self.queues.partial_freeze_blocks(kind) {
            debug!(
                conversation = %self.conversation_id,
                proposal_id = proposal.proposal_id,
                ?kind,
                "dropping lower-priority proposal: active emergency partial-freeze"
            );
            return Ok(());
        }

        let proposal_id = proposal.proposal_id;
        let expected_voters = proposal.expected_voters_count;
        let expiration_timestamp = proposal.expiration_timestamp;

        // Consensus validation runs before any queue mirroring, so a proposal
        // that fails it (expired or otherwise mis-stamped) leaves no local
        // trace — a crafted timestamp can't arm the partial freeze or park a
        // pending update.
        let scope = self.conversation_id.clone();
        let local_now = self.now.as_secs();
        if let Err(e) = self
            .consensus
            .process_incoming_proposal(&scope, proposal, local_now)
        {
            if matches!(e, ConsensusError::ProposalExpired) {
                self.emit(Event::Error {
                    operation: "incoming_proposal".to_string(),
                    message: format!(
                        "proposal {proposal_id} expired on arrival \
                         (expiration {expiration_timestamp}, local now {local_now}); \
                         possible clock skew"
                    ),
                });
            }
            return Err(e.into());
        }
        // store: consensus sessions (WP3g)

        if let Some(req) = decoded.as_ref() {
            let current_epoch = self.epoch;
            // In flight the moment it arrives, so the steward's drain won't
            // re-propose it.
            self.queues.track_voting_proposal(proposal_id, req);
            match &req.payload {
                Some(conversation_update_request::Payload::EmergencyCriteria(_)) => {
                    self.queues.insert_emergency(proposal_id);
                }
                Some(conversation_update_request::Payload::MemberInvite(_))
                | Some(conversation_update_request::Payload::RemoveMember(_)) => {
                    // RFC §Buffering KeyPackages: hold the change until a
                    // commit applies it, so a later steward can retry it if
                    // this round never lands.
                    self.queues
                        .insert_pending_update(req.clone(), current_epoch);
                }
                _ => {}
            }
        }

        if expected_voters > 1 {
            match decoded.as_ref().and_then(|r| r.payload.as_ref()) {
                Some(conversation_update_request::Payload::StewardElection(election)) => {
                    // Recomputed locally, so no vote request reaches the
                    // router: the verdict is deterministic.
                    let verdict = self.validate_election_list(election)?;
                    self.register_auto_vote(proposal_id, Duration::ZERO, verdict);
                }
                Some(conversation_update_request::Payload::MemberInvite(invite)) => {
                    // The vote waits for the router's verdict on the key
                    // package; no auto-vote is armed until then, so silence
                    // cannot approve an unvalidated joiner.
                    let member = MemberId::from(invite.member_id.clone());
                    let key_package = invite.key_package_bytes.clone();
                    self.decide(Decision::ValidateKeyPackage {
                        proposal_id,
                        member,
                        key_package,
                    });
                }
                _ => {
                    if let Some(request) = decoded {
                        self.emit(Event::VoteRequested {
                            proposal_id,
                            request,
                        });
                    }
                    let delay = self.config.voting_delay;
                    let vote = self.config.liveness_criteria_yes;
                    self.register_auto_vote(proposal_id, delay, vote);
                }
            }
            self.register_consensus_timeout(proposal_id, self.config.consensus_timeout);
        }
        Ok(())
    }

    /// The router validated the key package of the invite proposal
    /// `proposal_id`; `valid` says whether it passed and matches the claimed
    /// member id.
    ///
    /// A pass releases the vote — the request reaches the router and the
    /// auto-vote timer arms. A failure votes NO right away, so a bad key
    /// package costs one round instead of a timeout.
    pub fn key_package_checked(
        &mut self,
        now: Timestamp,
        proposal_id: u32,
        valid: bool,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        if !valid {
            info!(
                conversation = %self.conversation_id,
                proposal_id, "invite rejected: key package failed validation"
            );
            if let Err(e) = self.cast_local_vote(proposal_id, false) {
                self.report_failure("key_package_checked", &e);
            }
            return Ok(self.finish());
        }

        let scope = self.conversation_id.clone();
        let stored = self
            .consensus
            .storage()
            .get_active_proposals(&scope)?
            .into_iter()
            .find(|p| p.proposal_id == proposal_id);
        let Some(stored) = stored else {
            debug!(
                conversation = %self.conversation_id,
                proposal_id,
                "key-package verdict dropped: the session is no longer active"
            );
            return Ok(self.finish());
        };
        let request = ConversationUpdateRequest::decode(stored.payload.as_slice())?;
        self.emit(Event::VoteRequested {
            proposal_id,
            request,
        });
        let delay = self.config.voting_delay;
        let vote = self.config.liveness_criteria_yes;
        self.register_auto_vote(proposal_id, delay, vote);
        Ok(self.finish())
    }

    // ── deadlines ────────────────────────────────────────────────────

    /// Fire the auto-votes and consensus timeouts that `now` has reached.
    /// Per-proposal errors are logged and skipped so one stuck proposal
    /// can't block the rest.
    pub(crate) fn tick_deadlines(&mut self) {
        let now = self.now;
        let auto_votes_due: Vec<(u32, bool)> = self
            .timing
            .pending_auto_votes
            .iter()
            .filter(|(_, e)| e.fire_at <= now)
            .map(|(id, e)| (*id, e.vote))
            .collect();
        for (id, _) in &auto_votes_due {
            self.timing.pending_auto_votes.remove(id);
        }
        let timeouts_due: Vec<u32> = self
            .timing
            .pending_consensus_timeouts
            .iter()
            .filter(|(_, fire_at)| **fire_at <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in &timeouts_due {
            self.timing.pending_consensus_timeouts.remove(id);
        }

        for (proposal_id, vote) in auto_votes_due {
            if let Err(e) = self.cast_local_vote(proposal_id, vote) {
                match e {
                    ConversationError::Consensus(
                        ConsensusError::UserAlreadyVoted
                        | ConsensusError::SessionNotActive
                        | ConsensusError::SessionNotFound,
                    ) => debug!(
                        proposal_id,
                        error = %e,
                        "auto-vote skipped: already voted or session resolved"
                    ),
                    ConversationError::Consensus(ConsensusError::ProposalExpired) => {
                        warn!(
                            proposal_id,
                            error = %e,
                            "auto-vote dropped: proposal expired before the vote fired; \
                             possible clock skew"
                        );
                        self.report_failure("auto_vote", &e);
                    }
                    _ => {
                        warn!(proposal_id, error = %e, "auto-vote failed");
                        self.report_failure("auto_vote", &e);
                    }
                }
            }
        }
        for proposal_id in timeouts_due {
            self.resolve_on_timeout(proposal_id);
        }
    }

    // ── private ──────────────────────────────────────────────────────

    /// Whether this proposal can be opened right now, and how many voters it
    /// expects. Nothing opens while a round is running; an active emergency
    /// also blocks lower-priority proposals.
    fn check_proposal_allowed(&self, kind: ProposalKind) -> Result<u32, ConversationError> {
        let state = self.current_state();
        if state != ConversationState::Working {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }
        if self.queues.partial_freeze_blocks(kind) {
            return Err(ConversationError::PartialFreeze);
        }
        Ok(self.members.len() as u32)
    }

    /// Push a proposal whose deadline elapsed into the consensus library's
    /// timeout resolution. `SessionNotFound`/`SessionNotActive` despite the
    /// `still_active` guard means the session resolved in between — benign
    /// when the proposal is in the resolved cache, a logic bug worth a
    /// warning when it isn't.
    fn resolve_on_timeout(&self, proposal_id: u32) {
        let scope = self.conversation_id.clone();
        let still_active = self
            .consensus
            .storage()
            .get_active_proposals(&scope)
            .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
            .unwrap_or(false);
        if !still_active {
            return;
        }
        match self
            .consensus
            .handle_consensus_timeout(&scope, proposal_id, self.now.as_secs())
        {
            Ok(_) => {}
            Err(ConsensusError::SessionNotFound) | Err(ConsensusError::SessionNotActive) => {
                if self.queues.is_consensus_outcome_applied(proposal_id) {
                    debug!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "timeout fired for already-resolved proposal: ignoring"
                    );
                } else {
                    warn!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "timeout fired for unknown proposal id: no session and not in resolved cache"
                    );
                }
            }
            Err(e) => {
                info!(proposal_id, error = %e, "timeout resolution skipped");
            }
        }
    }

    /// Open a consensus session for `request`; returns the new proposal id
    /// and the unbundled `Proposal` wire message.
    fn submit_proposal(
        &self,
        request: &ConversationUpdateRequest,
        params: ProposalParams,
    ) -> Result<(u32, Proposal), ConversationError> {
        let create_request = CreateProposalRequest::new(
            uuid::Uuid::new_v4().to_string(),
            request.encode_to_vec(),
            self.own.clone(),
            params.expected_voters,
            params.proposal_expiration.as_secs(),
            params.liveness_criteria_yes,
        )?;

        let scope = self.conversation_id.clone();
        let proposal = self.consensus.create_proposal_with_config(
            &scope,
            create_request,
            Some(ConsensusConfig::gossipsub().with_timeout(params.consensus_timeout)?),
            self.now.as_secs(),
        )?;

        info!(
            conversation = %self.conversation_id,
            proposal_id = proposal.proposal_id,
            voters = params.expected_voters,
            "proposal opened"
        );

        let proposal_id = proposal.proposal_id;
        Ok((proposal_id, proposal))
    }

    /// Open a self-leave session: a hand-crafted `Proposal` carrying the
    /// deterministic [`self_leave_proposal_id`] and the leaver's YES, so the
    /// single-voter session resolves on arrival.
    ///
    /// The fixed id makes retransmits collide as `ProposalAlreadyExist`,
    /// returned as `Ok(None)` — nothing to broadcast.
    fn submit_self_leave_proposal(
        &self,
        params: ProposalParams,
    ) -> Result<Option<Proposal>, ConversationError> {
        let request = ConversationUpdateRequest::remove_member(self.own.clone());
        let payload = request.encode_to_vec();

        let now = self.now.as_secs();
        let expiration = now.saturating_add(params.proposal_expiration.as_secs());

        let proposal_id = self_leave_proposal_id(&self.own);
        let mut proposal = Proposal {
            name: format!("self-leave:{proposal_id}"),
            payload,
            proposal_id,
            proposal_owner: self.own.clone(),
            votes: Vec::new(),
            expected_voters_count: params.expected_voters,
            round: 1,
            timestamp: now,
            expiration_timestamp: expiration,
            liveness_criteria_yes: params.liveness_criteria_yes,
        };

        let yes_vote = build_vote(&proposal, true, self.consensus.signer(), now)?;
        proposal.votes.push(yes_vote);

        let scope = self.conversation_id.clone();
        match self
            .consensus
            .process_incoming_proposal(&scope, proposal.clone(), now)
        {
            Ok(()) => {
                info!(
                    conversation = %self.conversation_id,
                    proposal_id, "self-leave proposal opened (expected_voters=1, bundled YES)"
                );
                Ok(Some(proposal))
            }
            Err(ConsensusError::ProposalAlreadyExist) => {
                info!(
                    conversation = %self.conversation_id,
                    proposal_id, "self-leave already in flight, skipping retransmit"
                );
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ConversationConfig,
        engine::{store::InMemoryStore, types::Outbound},
        protos::de_mls::messages::v1::MemberInvite,
    };

    fn member(id: u8) -> Vec<u8> {
        vec![id; 8]
    }

    /// A four-member engine at epoch 1, owned by member 1, ready to be
    /// driven at `Timestamp::ZERO`.
    fn engine() -> Engine<InMemoryStore> {
        let members: Vec<MemberId> = (1..=4).map(|i| MemberId::from(member(i))).collect();
        let (mut engine, _) = Engine::create(
            "conv",
            MemberId::from(member(1)),
            1,
            &members,
            ConversationConfig::default(),
            InMemoryStore::default(),
        )
        .expect("engine");
        engine.begin(Timestamp::ZERO);
        engine
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
    /// well inside its expiration window.
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

    /// A proposal whose payload claims an owner other than the sender the
    /// MLS layer authenticated is forged: it leaves no trace at all.
    #[test]
    fn proposal_from_a_different_sender_is_dropped() {
        let mut engine = engine();
        let request = ConversationUpdateRequest::remove_member(member(4));
        let proposal = peer_proposal(&member(2), 7, &request, 4);

        engine
            .on_incoming_proposal(&member(3), proposal)
            .expect("dropped, not an error");

        assert!(engine.out.events.is_empty());
        assert!(engine.out.decisions.is_empty());
        assert!(!engine.queues.is_voting(7));
    }

    /// `expected_voters == 1` bypasses peer voting, so it is allowed only
    /// when the sender removes itself.
    #[test]
    fn single_voter_proposal_is_only_allowed_for_self_removal() {
        let mut engine = engine();
        let request = ConversationUpdateRequest::remove_member(member(4));
        let proposal = peer_proposal(&member(2), 8, &request, 1);

        engine
            .on_incoming_proposal(&member(2), proposal)
            .expect("dropped, not an error");

        assert!(!engine.queues.is_voting(8));

        let request = ConversationUpdateRequest::remove_member(member(2));
        let proposal = peer_proposal(&member(2), 9, &request, 1);
        engine
            .on_incoming_proposal(&member(2), proposal)
            .expect("self-removal accepted");
        assert!(engine.queues.is_voting(9));
    }

    /// An invite asks the router for a key-package verdict and arms nothing:
    /// no vote request, no auto-vote. Silence must not approve a joiner
    /// nobody validated.
    #[test]
    fn incoming_invite_asks_for_validation_before_voting() {
        let mut engine = engine();
        let request = invite_request(&member(9));
        let proposal = peer_proposal(&member(2), 11, &request, 4);

        engine.on_incoming_proposal(&member(2), proposal).unwrap();

        assert!(matches!(
            engine.out.decisions.as_slice(),
            [Decision::ValidateKeyPackage {
                proposal_id: 11,
                ..
            }]
        ));
        assert!(
            !engine
                .out
                .events
                .iter()
                .any(|e| matches!(e, Event::VoteRequested { .. }))
        );
        assert!(!engine.timing.pending_auto_votes.contains_key(&11));
        assert!(engine.timing.pending_consensus_timeouts.contains_key(&11));
    }

    /// A passing key package releases the vote: the request reaches the
    /// router and the auto-vote timer arms.
    #[test]
    fn key_package_pass_releases_the_vote() {
        let mut engine = engine();
        let request = invite_request(&member(9));
        let proposal = peer_proposal(&member(2), 12, &request, 4);
        engine.on_incoming_proposal(&member(2), proposal).unwrap();
        engine.out = Output::default();

        let out = engine
            .key_package_checked(Timestamp::ZERO, 12, true)
            .unwrap();

        assert!(
            out.events.iter().any(
                |e| matches!(e, Event::VoteRequested { proposal_id, .. } if *proposal_id == 12)
            )
        );
        assert!(engine.timing.pending_auto_votes.contains_key(&12));
    }

    /// A failing key package votes NO right away rather than waiting the
    /// session out.
    #[test]
    fn key_package_failure_votes_no() {
        let mut engine = engine();
        let request = invite_request(&member(9));
        let proposal = peer_proposal(&member(2), 13, &request, 4);
        engine.on_incoming_proposal(&member(2), proposal).unwrap();
        engine.out = Output::default();

        let out = engine
            .key_package_checked(Timestamp::ZERO, 13, false)
            .unwrap();

        assert!(matches!(out.outbound.as_slice(), [Outbound::Control(_)]));
        assert!(!engine.timing.pending_auto_votes.contains_key(&13));
    }

    /// A vote whose owner is not the authenticated sender never reaches the
    /// session.
    #[test]
    fn vote_from_a_different_sender_is_dropped() {
        let mut engine = engine();
        let request = ConversationUpdateRequest::remove_member(member(4));
        let proposal = peer_proposal(&member(2), 14, &request, 4);
        engine.on_incoming_proposal(&member(2), proposal).unwrap();

        let vote = Vote {
            proposal_id: 14,
            vote: true,
            vote_owner: member(2),
            ..Default::default()
        };
        engine
            .forward_incoming_vote(&member(3), vote)
            .expect("dropped, not an error");

        let active = engine
            .consensus
            .storage()
            .get_active_proposals(&"conv".to_string())
            .unwrap();
        let session = active.iter().find(|p| p.proposal_id == 14).unwrap();
        assert!(session.votes.is_empty(), "forged vote must not be counted");
    }

    /// A proposal the local emergency freeze outranks is dropped outright —
    /// not buffered, not forwarded, not surfaced.
    #[test]
    fn partial_freeze_drops_lower_priority_proposals() {
        let mut engine = engine();
        engine.queues.insert_emergency(1);
        let request = ConversationUpdateRequest::remove_member(member(4));
        let proposal = peer_proposal(&member(2), 15, &request, 4);

        engine.on_incoming_proposal(&member(2), proposal).unwrap();

        assert!(!engine.queues.is_voting(15));
        assert!(engine.out.events.is_empty());
    }

    /// The auto-vote timer fires once its deadline passes and puts one Vote
    /// control message on the wire.
    #[test]
    fn auto_vote_fires_at_its_deadline() {
        let mut engine = engine();
        let request = ConversationUpdateRequest::remove_member(member(4));
        let proposal = peer_proposal(&member(2), 16, &request, 4);
        engine.on_incoming_proposal(&member(2), proposal).unwrap();
        engine.out = Output::default();

        let fire_at = engine.timing.pending_auto_votes[&16].fire_at;
        engine.begin(fire_at);
        engine.tick_deadlines();

        assert!(matches!(
            engine.out.outbound.as_slice(),
            [Outbound::Control(_)]
        ));
        assert!(engine.timing.pending_auto_votes.is_empty());
    }
}
