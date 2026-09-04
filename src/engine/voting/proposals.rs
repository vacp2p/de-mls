//! Opening proposals, casting votes, and the adapters that talk to the
//! consensus service.
//!
//! Everything runs inline on the driving call: opening a proposal starts the
//! consensus session right away, while the time-based follow-ups (auto-votes,
//! consensus timeouts) wait as deadlines that `tick_deadlines` fires on a
//! later tick — see [`crate::engine::voting::deadlines`]. Wire-level
//! construction (control-message encoding, session opening) lives in
//! [`crate::engine::voting::wire`]. Wire messages leave as
//! `Outbound::Control` bytes the router seals; nothing here touches a group.

use std::time::Duration;

use hashgraph_like_consensus::{
    error::ConsensusError,
    protos::consensus::v1::{Proposal, Vote},
    storage::ConsensusStorage,
};
use prost::Message;
use tracing::{debug, info, warn};

use crate::{
    ConversationError,
    engine::{
        handle::Engine,
        proposal_kind::ProposalKind,
        store::EngineStore,
        types::{Decision, Event, MemberId, Output, Phase, Timestamp},
        util::in_flight_target,
        voting::wire::{
            ProposalParams, authorize_fast_path_proposal, control_proposal, control_vote,
        },
    },
    protos::de_mls::messages::v1::{ConversationUpdateRequest, conversation_update_request},
};

/// The creator's intent at proposal submit time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreatorVote {
    /// Bundle a YES vote with the proposal in one atomic wire message; no vote
    /// request reaches the router, since the vote is already cast. For actions
    /// where submitting already expresses the vote (member add, remove,
    /// self-executing protocol moves).
    Yes,
    /// Broadcast the proposal unbundled and treat the creator like any
    /// other voter: `VoteRequested` plus the auto-vote timer. For steward
    /// auto-propose paths, where the steward forwards peer intent without
    /// endorsing it.
    Deferred,
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

    // ── private ──────────────────────────────────────────────────────

    /// Whether this proposal can be opened right now, and how many voters it
    /// expects. Nothing opens while a round is running; an active emergency
    /// also blocks lower-priority proposals.
    fn check_proposal_allowed(&self, kind: ProposalKind) -> Result<u32, ConversationError> {
        let state = self.phase;
        if state != Phase::Working {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }
        if self.queues.partial_freeze_blocks(kind) {
            return Err(ConversationError::PartialFreeze);
        }
        Ok(self.members.len() as u32)
    }
}
