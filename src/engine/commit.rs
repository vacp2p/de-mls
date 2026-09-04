//! The commit round over facts: the per-epoch candidate buffer, the cheap
//! admission gate the router asks before staging, the epoch-steward lookup
//! that picks the round's one acceptable candidate, and the merge report
//! that moves the engine's own view of the group.
//!
//! Only the epoch steward's candidate may win a round (design 11): every
//! other buffered candidate is rejected and scored, whatever it carries.
//! Nothing here stages, merges or discards anything itself. The router owns
//! the group; the engine names a hash and the router reports back through
//! [`Engine::commit_applied`] or [`Engine::decision_failed`].

use prost::Message;
use tracing::{debug, info, warn};

use crate::{
    ConversationError, ScoreEvent, ScoreOp,
    engine::{
        handle::{Engine, PendingBuild, PendingMerge},
        proposal_kind::ProposalKind,
        queues::{EarlyCandidate, EngineQueues},
        store::EngineStore,
        types::{
            Action, Admission, CommitHash, Decision, DecisionFailure, Event, MemberId,
            MembershipDelta, Outbound, Output, Phase, StagedFacts, Timestamp,
        },
    },
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, conversation_update_request::Payload,
    },
};

// ═══════════════════════════════════════════════════════════════════════════
// The buffer
// ═══════════════════════════════════════════════════════════════════════════

/// One candidate in the round: the envelope as it came off the wire and the
/// facts the router learned by staging its commit.
#[derive(Clone, Debug)]
struct BufferedCandidate {
    hash: CommitHash,
    envelope: CommitCandidate,
    /// `None` for our own candidate: we never stage what we built. Its facts
    /// are the envelope's `proposal_count` and the batch in `pending_build`.
    facts: Option<StagedFacts>,
    is_local: bool,
}

impl BufferedCandidate {
    /// The commit's author. The MLS-authenticated sender for a remote
    /// candidate; `own` for ours.
    fn author(&self, own: &[u8]) -> Vec<u8> {
        match self.facts.as_ref() {
            Some(facts) => facts.sender.as_bytes().to_vec(),
            None => own.to_vec(),
        }
    }
}

/// The candidates collected for one epoch, plus the envelopes admitted for
/// staging whose facts have not come back yet. A candidate for a different
/// epoch starts a fresh round: the previous one's entries are stale.
#[derive(Clone, Debug, Default)]
pub(crate) struct CommitRoundBuffer {
    epoch: u64,
    candidates: Vec<BufferedCandidate>,
    /// Admitted envelopes waiting for the router's `handle_candidate`.
    awaiting: Vec<(CommitHash, CommitCandidate)>,
}

impl CommitRoundBuffer {
    /// Point the buffer at `epoch`, clearing a previous round's entries.
    fn open(&mut self, epoch: u64) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.candidates.clear();
            self.awaiting.clear();
        }
    }

    /// Whether `hash` is buffered or awaiting facts for `epoch`.
    fn knows(&self, epoch: u64, hash: &CommitHash) -> bool {
        self.epoch == epoch
            && (self.candidates.iter().any(|c| c.hash == *hash)
                || self.awaiting.iter().any(|(h, _)| h == hash))
    }

    /// Candidates plus outstanding admissions, against which the per-round
    /// one-per-member cap is measured.
    fn occupancy(&self, epoch: u64) -> usize {
        if self.epoch != epoch {
            return 0;
        }
        self.candidates.len() + self.awaiting.len()
    }

    /// Remember an admitted envelope until its facts arrive.
    fn await_facts(&mut self, epoch: u64, hash: CommitHash, envelope: CommitCandidate) {
        self.open(epoch);
        self.awaiting.push((hash, envelope));
    }

    /// Take the envelope admitted under `hash`.
    fn take_awaited(&mut self, hash: &CommitHash) -> Option<CommitCandidate> {
        let at = self.awaiting.iter().position(|(h, _)| h == hash)?;
        Some(self.awaiting.remove(at).1)
    }

    /// Buffer a candidate for `epoch`, deduping by hash and capping at `max`
    /// (one per member; beyond that a distinct candidate is a fork).
    fn insert(&mut self, epoch: u64, candidate: BufferedCandidate, max: usize) -> bool {
        self.open(epoch);
        if self.candidates.iter().any(|c| c.hash == candidate.hash) {
            return false;
        }
        if !candidate.is_local && self.candidates.len() >= max {
            return false;
        }
        self.candidates.push(candidate);
        true
    }

    /// The candidates collected for `epoch`.
    fn candidates(&self, epoch: u64) -> &[BufferedCandidate] {
        if self.epoch != epoch {
            return &[];
        }
        &self.candidates
    }

    /// Every buffered hash, for the discard that ends a round.
    pub(crate) fn hashes(&self, epoch: u64) -> Vec<CommitHash> {
        self.candidates(epoch).iter().map(|c| c.hash).collect()
    }

    /// Drop one candidate: the router could not merge it.
    fn remove(&mut self, hash: &CommitHash) {
        self.candidates.retain(|c| c.hash != *hash);
    }

    pub(crate) fn clear(&mut self) {
        self.candidates.clear();
        self.awaiting.clear();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// The round snapshot
// ═══════════════════════════════════════════════════════════════════════════

/// The winner of one round and what the same decision discards.
pub(crate) struct RoundWinner {
    hash: CommitHash,
    facts: Option<StagedFacts>,
    is_local: bool,
    losers: Vec<CommitHash>,
}

/// The validation ladder's answer for one candidate.
enum CandidateVerdict {
    /// Merge it.
    Accept,
    /// Skip to the next, penalising its author when the failure is one.
    Reject(Option<ScoreOp>),
}

/// The membership change an action performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ActionKind {
    Add,
    Remove,
}

impl<St: EngineStore> Engine<St> {
    // ── admission and buffering ────────────────────────────────────────

    /// A `CommitCandidate` envelope arrived. Cheap checks only: shape, the
    /// conversation it names, the committed-history window, and the
    /// per-round cap. The epoch is the group's business, checked when the
    /// router stages.
    pub fn admit_candidate(
        &mut self,
        now: Timestamp,
        envelope: &[u8],
    ) -> Result<Admission, ConversationError> {
        self.begin(now);
        let Ok(envelope) = CommitCandidate::decode(envelope) else {
            debug!(conversation = %self.conversation_id, "candidate dropped: malformed envelope");
            return Ok(Admission::Drop);
        };
        if envelope.conversation_id != self.conversation_id.as_bytes() {
            debug!(conversation = %self.conversation_id, "candidate dropped: other conversation");
            return Ok(Admission::Drop);
        }
        if envelope.proposal_count == 0 || envelope.commit_message.is_empty() {
            debug!(conversation = %self.conversation_id, "candidate dropped: empty commit or zero claimed proposals");
            return Ok(Admission::Drop);
        }
        if envelope.steward_member_id.is_empty() {
            debug!(conversation = %self.conversation_id, "candidate dropped: empty steward_member_id");
            return Ok(Admission::Drop);
        }
        let hash = CommitHash::of(&envelope.commit_message);
        if self.queues.has_committed_hash(&hash) {
            debug!(conversation = %self.conversation_id, "candidate dropped: already committed");
            return Ok(Admission::Drop);
        }
        let epoch = self.epoch;
        if self.round.knows(epoch, &hash) {
            debug!(conversation = %self.conversation_id, "candidate dropped: already in the round");
            return Ok(Admission::Drop);
        }
        if self.round.occupancy(epoch) >= self.members.len() {
            debug!(conversation = %self.conversation_id, "candidate dropped: round is full");
            return Ok(Admission::Drop);
        }
        let commit = envelope.commit_message.clone();
        self.round.await_facts(epoch, hash, envelope);
        Ok(Admission::Stage { hash, commit })
    }

    /// The facts the router learned by staging the commit admitted under
    /// `hash`. A candidate that arrives before the local node approved the
    /// proposals it carries is stashed and replayed once approval lands.
    pub fn handle_candidate(
        &mut self,
        now: Timestamp,
        hash: CommitHash,
        facts: StagedFacts,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        let Some(envelope) = self.round.take_awaited(&hash) else {
            debug!(conversation = %self.conversation_id, ?hash, "facts for a candidate the round never admitted");
            return Ok(self.finish());
        };

        // The consensus outcome can still be in flight: a peer steward may
        // reach consensus and commit before our own vote lands. Stashing keeps
        // us from falling an epoch behind.
        if self.queues.approved_proposals_count() == 0 {
            let max = self.members.len();
            self.queues.stash_early_candidate(
                self.epoch,
                EarlyCandidate {
                    hash,
                    envelope,
                    facts,
                },
                max,
            );
            debug!(conversation = %self.conversation_id, "candidate stashed: proposals not approved yet");
            return Ok(self.finish());
        }

        let epoch = self.epoch;
        let max = self.members.len();
        let steward = envelope.steward_member_id.clone();
        self.round.insert(
            epoch,
            BufferedCandidate {
                hash,
                envelope,
                facts: Some(facts),
                is_local: false,
            },
            max,
        );
        debug!(conversation = %self.conversation_id, steward = ?steward, "candidate buffered");

        self.open_round_on_peer_candidate()?;
        self.report_round_progress();
        Ok(self.finish())
    }

    /// A peer's candidate opens the round locally: from `Working` we enter
    /// `Freezing` and, if we are the epoch steward, mint our own.
    fn open_round_on_peer_candidate(&mut self) -> Result<(), ConversationError> {
        if self.current_state() != Phase::Working {
            return Ok(());
        }
        let Some(event) = self.start_freezing() else {
            return Ok(());
        };
        self.on_freeze_entered(event)
    }

    /// Emit [`Event::CommitRoundProgress`] when the count moved.
    fn report_round_progress(&mut self) {
        if self.current_state() != Phase::Freezing {
            return;
        }
        let progress = self.commit_candidate_count();
        if self.timing.last_commit_round_progress == Some(progress) {
            return;
        }
        self.timing.last_commit_round_progress = Some(progress);
        let (received, expected) = progress;
        self.emit(Event::CommitRoundProgress { received, expected });
    }

    /// Re-buffer candidates stashed before their proposal was approved.
    pub(crate) fn replay_early_candidates(&mut self) -> Result<(), ConversationError> {
        let epoch = self.epoch;
        let stashed = self.queues.take_early_candidates(epoch);
        if stashed.is_empty() {
            return Ok(());
        }
        let max = self.members.len();
        for candidate in stashed {
            if self.queues.has_committed_hash(&candidate.hash) {
                continue;
            }
            self.round.insert(
                epoch,
                BufferedCandidate {
                    hash: candidate.hash,
                    envelope: candidate.envelope,
                    facts: Some(candidate.facts),
                    is_local: false,
                },
                max,
            );
        }
        Ok(())
    }

    /// `(received, expected)` candidates for the current round.
    pub(crate) fn commit_candidate_count(&self) -> (usize, usize) {
        let received = self.round.candidates(self.epoch).len();
        let expected = self
            .steward_list
            .current_list()
            .map_or(0, |list| list.len());
        (received, expected)
    }

    // ── minting our own candidate ──────────────────────────────────────

    /// Ask the router to build our candidate from the approved batch. Only
    /// the epoch steward builds (design 11): `Ok(false)` when we aren't it,
    /// there is nothing to commit, or a build is already outstanding.
    pub(crate) fn request_own_candidate(&mut self) -> Result<bool, ConversationError> {
        if self.pending_build.is_some() {
            return Ok(false);
        }
        if !self.is_epoch_steward() {
            return Ok(false);
        }
        if self.queues.approved_proposals().is_empty() {
            return Err(ConversationError::NoProposals);
        }
        // Governance proposals are consensus-only and never reach a commit.
        let governance: Vec<u32> = self
            .queues
            .approved_proposals()
            .iter()
            .filter(|(_, req)| ProposalKind::of(req).is_governance())
            .map(|(&id, _)| id)
            .collect();
        if !governance.is_empty() {
            return Err(ConversationError::UnexpectedNonMlsProposals {
                proposal_ids: governance,
            });
        }

        let actions = self.batch_actions()?;
        if actions.is_empty() {
            return Ok(false);
        }
        info!(
            conversation = %self.conversation_id,
            epoch = self.epoch,
            proposals = actions.len(),
            "commit candidate requested"
        );
        self.pending_build = Some(PendingBuild {
            actions: actions.clone(),
        });
        self.decide(Decision::BuildCommit { actions });
        Ok(true)
    }

    /// The approved queue projected to commit actions, in FIFO order and
    /// capped at `commit_batch_max`. An urgent (emergency-driven) freeze
    /// narrows the batch to the target's removal alone.
    fn batch_actions(&self) -> Result<Vec<Action>, ConversationError> {
        let urgent = self.queues.urgent_commit_target();
        let k_max = self.config.commit_batch_max;
        let approved = self.queues.approved_proposals();
        let mut actions = Vec::with_capacity(approved.len().min(k_max));
        for (_id, proposal) in approved.iter().take(k_max) {
            match proposal.payload.as_ref() {
                Some(Payload::MemberInvite(invite)) => {
                    if urgent.is_some() || self.is_member(&invite.member_id) {
                        continue;
                    }
                    actions.push(Action::Add {
                        member: MemberId::from(invite.member_id.as_slice()),
                        key_package: invite.key_package_bytes.clone(),
                    });
                }
                Some(Payload::RemoveMember(remove)) => {
                    if urgent.is_some_and(|target| remove.member_id != target)
                        || !self.is_member(&remove.member_id)
                    {
                        continue;
                    }
                    actions.push(Action::Remove {
                        member: MemberId::from(remove.member_id.as_slice()),
                    });
                }
                _ => return Err(ConversationError::InvalidConversationUpdateRequest),
            }
        }
        Ok(actions)
    }

    /// The router built the commit a `BuildCommit` decision asked for and
    /// keeps it pending. Buffer it as ours and broadcast the envelope.
    pub fn own_candidate_built(
        &mut self,
        now: Timestamp,
        hash: CommitHash,
        proposal_count: u32,
        commit: Vec<u8>,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        let Some(build) = self.pending_build.take() else {
            self.emit(Event::Error {
                operation: "own_candidate_built".to_owned(),
                message: "no commit build was outstanding".to_owned(),
            });
            return Ok(self.finish());
        };
        // The group may skip an action (a remove of someone already gone),
        // never add one.
        if proposal_count as usize > build.actions.len() {
            self.emit(Event::Error {
                operation: "own_candidate_built".to_owned(),
                message: "the built commit carries more proposals than the batch".to_owned(),
            });
            return Ok(self.finish());
        }
        let envelope = CommitCandidate {
            conversation_id: self.conversation_id.as_bytes().to_vec(),
            commit_message: commit,
            steward_member_id: self.own.clone(),
            proposal_count,
        };
        let epoch = self.epoch;
        let max = self.members.len();
        self.round.insert(
            epoch,
            BufferedCandidate {
                hash,
                envelope: envelope.clone(),
                facts: None,
                is_local: true,
            },
            max,
        );
        self.send_candidate(envelope.encode_to_vec());
        self.report_round_progress();
        Ok(self.finish())
    }

    /// Finish entering `Freezing`: ask the router for our own candidate — a
    /// no-op unless we are the epoch steward — then announce the phase.
    pub(crate) fn on_freeze_entered(&mut self, event: Phase) -> Result<(), ConversationError> {
        if let Err(e) = self.request_own_candidate() {
            // A mint failure stalls this round and repeats on every retry, and
            // the phase change alone reads as a healthy freeze.
            self.report_failure("commit_candidate_build", &e);
        }
        self.emit_phase(Some(event));
        Ok(())
    }

    // ── selection ──────────────────────────────────────────────────────

    /// The eligibility-filtered epoch steward for the current epoch — the
    /// only member whose candidate this round may accept.
    fn expected_steward(&self) -> Option<Vec<u8>> {
        let eligible = self.queues.steward_eligibility(&self.members);
        self.steward_list
            .epoch_steward(self.epoch, &eligible)
            .map(<[u8]>::to_vec)
    }

    /// Walk the round's candidates and accept the epoch steward's alone.
    /// Every other candidate — a different author, or an author claiming the
    /// role with a fork — is scored `MisbehavingCommit` and discarded. The
    /// epoch steward's candidate goes through the validation ladder (wire
    /// claims against the authenticated commit, actions against the voted
    /// set): valid scores `SuccessfulCommit` and wins; invalid scores its
    /// specific violation and is discarded like everyone else.
    pub(crate) fn select_epoch_steward_candidate(
        &mut self,
    ) -> Result<Option<RoundWinner>, ConversationError> {
        let expected = self.expected_steward();
        let candidates = self.round.candidates(self.epoch).to_vec();

        let mut ops: Vec<ScoreOp> = Vec::new();
        let mut losers: Vec<CommitHash> = Vec::new();
        let mut winner: Option<BufferedCandidate> = None;

        for candidate in candidates {
            let author = candidate.author(&self.own);
            if expected.as_deref() != Some(author.as_slice()) || winner.is_some() {
                ops.push(penalty(&author, ScoreEvent::MisbehavingCommit));
                losers.push(candidate.hash);
                continue;
            }
            match self.verdict(&candidate) {
                CandidateVerdict::Accept => {
                    ops.push(penalty(&author, ScoreEvent::SuccessfulCommit));
                    winner = Some(candidate);
                }
                CandidateVerdict::Reject(penalty) => {
                    ops.extend(penalty);
                    losers.push(candidate.hash);
                }
            }
        }

        if !ops.is_empty() {
            self.apply_score_ops(&ops);
        }

        Ok(winner.map(|w| RoundWinner {
            hash: w.hash,
            facts: w.facts,
            is_local: w.is_local,
            losers,
        }))
    }

    /// Queue the decisions one winner needs: discard the losers, then merge.
    pub(crate) fn decide_winner(&mut self, winner: RoundWinner) {
        self.pending_merge = Some(PendingMerge {
            hash: winner.hash,
            facts: winner.facts,
            is_local: winner.is_local,
        });
        if !winner.losers.is_empty() {
            self.decide(Decision::Discard {
                hashes: winner.losers,
            });
        }
        self.decide(Decision::Merge { hash: winner.hash });
    }

    /// The validation ladder for the epoch steward's candidate, over the
    /// facts the router staged: wire claims against the authenticated
    /// commit, the actions against the voted set.
    fn verdict(&self, candidate: &BufferedCandidate) -> CandidateVerdict {
        // Our own candidate is valid by construction: we built it from the
        // approved batch and the router staged nothing to contradict.
        if candidate.is_local {
            return CandidateVerdict::Accept;
        }
        let Some(facts) = candidate.facts.as_ref() else {
            return CandidateVerdict::Reject(None);
        };
        let sender = facts.sender.as_bytes();

        // The wire-claimed steward must be the member whose signature the
        // commit carries; a mismatch is attributed to the actual signer.
        if sender != candidate.envelope.steward_member_id.as_slice() {
            warn!(
                conversation = %self.conversation_id,
                "violation: wire steward_member_id doesn't match the commit sender"
            );
            return CandidateVerdict::Reject(Some(penalty(sender, ScoreEvent::BrokenCommit)));
        }
        // The wire-claimed count is checked against the staged commit.
        if facts.proposal_count != candidate.envelope.proposal_count {
            warn!(
                conversation = %self.conversation_id,
                claimed = candidate.envelope.proposal_count,
                staged = facts.proposal_count,
                "violation: claimed proposal count doesn't match the commit"
            );
            return CandidateVerdict::Reject(Some(penalty(sender, ScoreEvent::BrokenCommit)));
        }
        // More proposals than actions means the commit carries kinds the
        // engine has no vocabulary for, so the voted set can't be checked.
        if facts.proposal_count as usize != facts.actions.len() {
            warn!(
                conversation = %self.conversation_id,
                count = facts.proposal_count,
                actions = facts.actions.len(),
                "violation: commit carries proposals the round cannot read"
            );
            return CandidateVerdict::Reject(Some(penalty(sender, ScoreEvent::BrokenMlsProposal)));
        }
        if !actions_match_voted(&self.queues, &facts.actions) {
            warn!(
                conversation = %self.conversation_id,
                actions = ?facts.actions,
                "violation: commit actions don't match the voted proposals"
            );
            return CandidateVerdict::Reject(Some(penalty(sender, ScoreEvent::BrokenMlsProposal)));
        }
        CandidateVerdict::Accept
    }

    /// No candidate validated: drop what the round collected and report that
    /// the epoch steward missed its window. The approved batch stays queued
    /// — the router asks again on the next inactivity tick, or after
    /// [`Engine::request_recovery`] skips the silent steward.
    pub(crate) fn close_round_without_commit(&mut self) -> Result<(), ConversationError> {
        let expected = self.expected_steward();

        let hashes = self.round.hashes(self.epoch);
        self.round.clear();
        if !hashes.is_empty() {
            self.decide(Decision::Discard { hashes });
        }

        if let Some(steward) = expected.as_deref()
            && steward != self.own.as_slice()
        {
            self.apply_score_ops(&[ScoreOp {
                member_id: steward.to_vec(),
                event: ScoreEvent::CensorshipInactivity,
            }]);
        }

        self.emit(Event::CommitMissing {
            epoch: self.epoch,
            steward: expected.map(|s| MemberId::from(s.as_slice())),
        });

        let resumed = self.start_working();
        self.emit_phase(Some(resumed));
        Ok(())
    }

    // ── the merge report ───────────────────────────────────────────────

    /// The router merged `hash`; the group is now at `epoch` with `members`.
    /// The engine moves its own view only here.
    pub fn commit_applied(
        &mut self,
        now: Timestamp,
        hash: CommitHash,
        epoch: u64,
        members: &[MemberId],
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        let mut adopted: Vec<Vec<u8>> = members.iter().map(|m| m.as_bytes().to_vec()).collect();
        adopted.sort();
        adopted.dedup();

        let merged = match self.pending_merge.take() {
            Some(pending) if pending.hash == hash => pending,
            _ => {
                // The group is the truth even when the engine did not name
                // this commit: adopt its facts and say so.
                self.emit(Event::Error {
                    operation: "commit_applied".to_owned(),
                    message: format!("merged {hash:?} without a matching merge decision"),
                });
                self.epoch = epoch;
                self.members = adopted;
                self.round.clear();
                return Ok(self.finish());
            }
        };

        let delta = MembershipDelta::between(&self.members, &adopted);
        self.members = adopted;
        self.epoch = epoch;
        self.queues.insert_committed_hash(hash);
        self.queues.store_membership_delta(delta.clone());
        self.finalize_committed_batch(epoch);
        self.reconcile_membership_after_commit(&delta, epoch)?;

        // A commit that unseats us ends the conversation here: no steward
        // work applies to a group we are no longer in.
        let self_removed = merged.facts.as_ref().is_some_and(|f| f.self_removed)
            || delta.removed.contains(&self.own);
        if self_removed {
            info!(conversation = %self.conversation_id, "commit removed this member");
            self.cancel_all_auto_votes();
            self.round.clear();
            self.decide(Decision::Leave);
            return Ok(self.finish());
        }

        if let Err(e) = self.steward_list_housekeeping() {
            self.report_failure("steward_list_housekeeping", &e);
        }
        if let Err(e) = self.sync_scoring_members() {
            self.report_failure("sync_scoring_members", &e);
        }
        self.queues.clear_skipped();
        self.timing.last_commit_round_progress = None;
        let working = self.start_working();
        self.emit_phase(Some(working));
        self.round.clear();

        // Our own commit seated joiners: the bootstrap rides with the welcome
        // the router holds, sealed at the epoch this merge reached.
        if merged.is_local && !delta.added.is_empty() {
            match self.build_bootstrap() {
                Ok(Some(bytes)) => self.out.outbound.push(Outbound::Bootstrap {
                    for_commit: hash,
                    bytes,
                }),
                Ok(None) => {}
                Err(e) => self.report_failure("welcome_bootstrap", &e),
            }
        }
        Ok(self.finish())
    }

    /// Post-commit queue bookkeeping: stamp join epochs and clear the entries
    /// the commit resolved, before the steward-list reconcile reads settled
    /// membership.
    fn finalize_committed_batch(&mut self, epoch: u64) {
        self.queues.apply_membership_delta_bookkeeping(epoch);
        if let Some(target) = self.queues.take_urgent_commit_target() {
            // Urgent commit: the rest of the queue waits for the next cycle.
            self.queues.drop_approved_removals_for(&target);
        } else {
            self.queues.drain_approved_proposals();
        }
        // store: proposals (WP3g)
    }

    /// Score each newly seated member from scratch and drop the departed, then
    /// report the change. A leaf whose member changed does both, so the
    /// incoming member starts clean.
    fn reconcile_membership_after_commit(
        &mut self,
        delta: &MembershipDelta,
        epoch: u64,
    ) -> Result<(), ConversationError> {
        let _ = self.queues.take_membership_delta();
        for member in &delta.removed {
            self.scoring.remove_member(member);
        }
        for member in &delta.added {
            self.scoring.add_member(member);
        }
        let max_epochs = self.config.pending_update_max_epochs;
        let _ = self.queues.expire_pending_updates(epoch, max_epochs);
        // store: proposals (WP3g)
        if !delta.is_empty() {
            self.emit(Event::MembersChanged {
                added: delta
                    .added
                    .iter()
                    .map(|m| MemberId::from(m.as_slice()))
                    .collect(),
                removed: delta
                    .removed
                    .iter()
                    .map(|m| MemberId::from(m.as_slice()))
                    .collect(),
            });
        }
        Ok(())
    }

    /// The router could not execute a decision. A failed merge closes the
    /// round with no commit; anything else is reported and dropped.
    pub fn decision_failed(
        &mut self,
        now: Timestamp,
        failure: DecisionFailure,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        let operation = match &failure.decision {
            Decision::Merge { hash } => {
                self.round.remove(hash);
                self.pending_merge = None;
                "merge"
            }
            Decision::BuildCommit { .. } => {
                self.pending_build = None;
                "build_commit"
            }
            Decision::Discard { .. } => "discard",
            Decision::Leave => "leave",
            Decision::ValidateKeyPackage { .. } => "validate_key_package",
        };
        self.emit(Event::Error {
            operation: operation.to_owned(),
            message: failure.reason,
        });

        if matches!(failure.decision, Decision::Merge { .. }) {
            self.close_round_without_commit()?;
        }
        Ok(self.finish())
    }
}

/// A penalty against the authenticated author of a candidate.
fn penalty(member_id: &[u8], event: ScoreEvent) -> ScoreOp {
    ScoreOp {
        member_id: member_id.to_vec(),
        event,
    }
}

/// Whether a commit's actions are exactly the membership changes the group
/// voted to approve.
///
/// Both sides are compared as sorted, deduplicated `(kind, member)`
/// projections: any member may open a proposal, so several approvals can name
/// the same change while the commit carries it once.
fn actions_match_voted(queues: &EngineQueues, actions: &[Action]) -> bool {
    let mut expected: Vec<(ActionKind, Vec<u8>)> = queues
        .approved_proposals()
        .values()
        .filter_map(voted_projection)
        .collect();
    let mut actual: Vec<(ActionKind, Vec<u8>)> = actions.iter().map(staged_projection).collect();
    expected.sort();
    expected.dedup();
    actual.sort();
    actual.dedup();
    expected == actual
}

/// `(kind, member)` projection of an approved request: an add keys on the
/// joiner's id as the proposer read it from the key package, a remove on its
/// target. `None` for governance payloads, which never reach a commit.
fn voted_projection(req: &ConversationUpdateRequest) -> Option<(ActionKind, Vec<u8>)> {
    match req.payload.as_ref()? {
        Payload::MemberInvite(invite) => Some((ActionKind::Add, invite.member_id.clone())),
        Payload::RemoveMember(remove) => Some((ActionKind::Remove, remove.member_id.clone())),
        _ => None,
    }
}

/// `(kind, member)` projection of a staged action, matching
/// [`voted_projection`]: one key space before and after a member is seated.
fn staged_projection(action: &Action) -> (ActionKind, Vec<u8>) {
    match action {
        Action::Add { member, .. } => (ActionKind::Add, member.as_bytes().to_vec()),
        Action::Remove { member } => (ActionKind::Remove, member.as_bytes().to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        engine::{config::EngineConfig, store::InMemoryStore, types::Timestamp},
        protos::de_mls::messages::v1::{MemberInvite, RemoveMember},
    };

    fn at(secs: u64) -> Timestamp {
        Timestamp::from_duration_since_epoch(Duration::from_secs(secs))
    }

    fn id(name: &str) -> MemberId {
        MemberId::from(name.as_bytes())
    }

    /// An engine over `members`, the first of which is this node. `create`
    /// installs the steward list over all of them at epoch 0.
    fn engine(members: &[&str]) -> Engine<InMemoryStore> {
        let ids: Vec<MemberId> = members.iter().map(|m| id(m)).collect();
        let (engine, _) = Engine::<InMemoryStore>::create(
            "conv",
            ids[0].clone(),
            0,
            &ids,
            EngineConfig::default(),
            InMemoryStore::default(),
        )
        .expect("create");
        engine
    }

    fn invite(joiner: &str) -> ConversationUpdateRequest {
        ConversationUpdateRequest {
            payload: Some(Payload::MemberInvite(MemberInvite {
                key_package_bytes: format!("kp:{joiner}").into_bytes(),
                member_id: joiner.as_bytes().to_vec(),
            })),
        }
    }

    fn removal(target: &str) -> ConversationUpdateRequest {
        ConversationUpdateRequest {
            payload: Some(Payload::RemoveMember(RemoveMember {
                member_id: target.as_bytes().to_vec(),
            })),
        }
    }

    fn envelope(steward: &str, count: u32, commit: &[u8]) -> CommitCandidate {
        CommitCandidate {
            conversation_id: b"conv".to_vec(),
            commit_message: commit.to_vec(),
            steward_member_id: steward.as_bytes().to_vec(),
            proposal_count: count,
        }
    }

    fn remote(steward: &str, count: u32, commit: &[u8]) -> BufferedCandidate {
        BufferedCandidate {
            hash: CommitHash::of(commit),
            envelope: envelope(steward, count, commit),
            facts: Some(StagedFacts {
                sender: id(steward),
                epoch: 0,
                actions: Vec::new(),
                proposal_count: count,
                self_removed: false,
            }),
            is_local: false,
        }
    }

    // ── the buffer ─────────────────────────────────────────────────────

    #[test]
    fn the_buffer_dedupes_by_hash_and_caps_at_one_per_member() {
        let mut buffer = CommitRoundBuffer::default();
        assert!(buffer.insert(0, remote("a", 1, b"aa"), 2));
        assert!(
            !buffer.insert(0, remote("a", 1, b"aa"), 2),
            "the same commit twice is one candidate"
        );
        assert!(buffer.insert(0, remote("b", 1, b"bb"), 2));
        assert!(
            !buffer.insert(0, remote("c", 1, b"cc"), 2),
            "beyond one per member a distinct candidate is a fork"
        );
        assert_eq!(buffer.candidates(0).len(), 2);
    }

    #[test]
    fn a_new_epoch_starts_a_fresh_round() {
        let mut buffer = CommitRoundBuffer::default();
        buffer.insert(0, remote("a", 1, b"aa"), 2);
        buffer.await_facts(0, CommitHash::of(b"bb"), envelope("b", 1, b"bb"));
        buffer.insert(1, remote("c", 1, b"cc"), 2);
        assert!(buffer.candidates(0).is_empty(), "epoch 0 is stale");
        assert_eq!(buffer.candidates(1).len(), 1);
        assert!(buffer.take_awaited(&CommitHash::of(b"bb")).is_none());
    }

    #[test]
    fn only_our_own_candidate_counts_as_local() {
        let mut buffer = CommitRoundBuffer::default();
        buffer.insert(0, remote("a", 1, b"aa"), 4);
        assert!(buffer.candidates(0).iter().all(|c| !c.is_local));
        let mut ours = remote("b", 1, b"bb");
        ours.is_local = true;
        ours.facts = None;
        buffer.insert(0, ours, 4);
        assert!(buffer.candidates(0).iter().any(|c| c.is_local));
    }

    // ── admission ──────────────────────────────────────────────────────

    #[test]
    fn admission_takes_a_well_formed_envelope_and_drops_the_rest() {
        let mut e = engine(&["alice", "bob"]);
        let good = envelope("bob", 1, b"commit").encode_to_vec();
        assert_eq!(
            e.admit_candidate(at(0), &good).unwrap(),
            Admission::Stage {
                hash: CommitHash::of(b"commit"),
                commit: b"commit".to_vec(),
            }
        );
        assert_eq!(
            e.admit_candidate(at(0), &good).unwrap(),
            Admission::Drop,
            "a second sighting of the same commit is already in the round"
        );

        let mut e = engine(&["alice", "bob"]);
        assert_eq!(
            e.admit_candidate(at(0), b"not a proto\xff\xff").unwrap(),
            Admission::Drop
        );
        assert_eq!(
            e.admit_candidate(at(0), &envelope("bob", 0, b"commit").encode_to_vec())
                .unwrap(),
            Admission::Drop,
            "a candidate claiming no proposals commits nothing"
        );
        assert_eq!(
            e.admit_candidate(at(0), &envelope("bob", 1, b"").encode_to_vec())
                .unwrap(),
            Admission::Drop
        );
        assert_eq!(
            e.admit_candidate(at(0), &envelope("", 1, b"commit").encode_to_vec())
                .unwrap(),
            Admission::Drop
        );
        let mut other = envelope("bob", 1, b"commit");
        other.conversation_id = b"elsewhere".to_vec();
        assert_eq!(
            e.admit_candidate(at(0), &other.encode_to_vec()).unwrap(),
            Admission::Drop
        );
    }

    #[test]
    fn an_already_committed_commit_is_never_re_admitted() {
        let mut e = engine(&["alice", "bob"]);
        e.queues.insert_committed_hash(CommitHash::of(b"commit"));
        assert_eq!(
            e.admit_candidate(at(0), &envelope("bob", 1, b"commit").encode_to_vec())
                .unwrap(),
            Admission::Drop
        );
    }

    #[test]
    fn a_candidate_ahead_of_its_approval_is_stashed_and_replayed() {
        let mut e = engine(&["alice", "bob"]);
        let admitted = e
            .admit_candidate(at(0), &envelope("bob", 1, b"commit").encode_to_vec())
            .unwrap();
        let Admission::Stage { hash, .. } = admitted else {
            panic!("expected a staging admission, got {admitted:?}");
        };
        let facts = StagedFacts {
            sender: id("bob"),
            epoch: 0,
            actions: vec![Action::Add {
                member: id("carol"),
                key_package: b"kp:carol".to_vec(),
            }],
            proposal_count: 1,
            self_removed: false,
        };
        e.handle_candidate(at(0), hash, facts).unwrap();
        assert_eq!(
            e.commit_candidate_count().0,
            0,
            "nothing approved yet, so the candidate waits outside the round"
        );

        e.queues.insert_approved_proposal(7, invite("carol"));
        e.replay_early_candidates().unwrap();
        assert_eq!(e.commit_candidate_count().0, 1);
    }

    // ── the validation ladder ──────────────────────────────────────────

    /// A claimed count that contradicts the staged commit is a violation
    /// pinned on the authenticated signer; an honest claim passes.
    #[test]
    fn a_lying_proposal_count_is_a_violation_and_an_honest_one_passes() {
        let mut e = engine(&["alice", "bob"]);
        e.queues.insert_approved_proposal(7, invite("carol"));
        let action = Action::Add {
            member: id("carol"),
            key_package: b"kp:carol".to_vec(),
        };

        let mut lying = remote("bob", 2, b"commit");
        lying.facts = Some(StagedFacts {
            sender: id("bob"),
            epoch: 0,
            actions: vec![action.clone()],
            proposal_count: 1,
            self_removed: false,
        });
        match e.verdict(&lying) {
            CandidateVerdict::Reject(Some(op)) => {
                assert_eq!(op.member_id, b"bob");
                assert_eq!(op.event, ScoreEvent::BrokenCommit);
            }
            _ => panic!("a claim that contradicts the commit must be rejected"),
        }

        let mut honest = remote("bob", 1, b"commit");
        honest.facts = Some(StagedFacts {
            sender: id("bob"),
            epoch: 0,
            actions: vec![action],
            proposal_count: 1,
            self_removed: false,
        });
        assert!(matches!(e.verdict(&honest), CandidateVerdict::Accept));
    }

    /// The wire id is forgeable, so it is checked against the signer and the
    /// penalty follows the signer.
    #[test]
    fn a_forged_steward_id_is_pinned_on_the_signer() {
        let mut e = engine(&["alice", "bob"]);
        e.queues.insert_approved_proposal(7, invite("carol"));
        let mut forged = remote("alice", 1, b"commit");
        forged.facts = Some(StagedFacts {
            sender: id("bob"),
            epoch: 0,
            actions: Vec::new(),
            proposal_count: 1,
            self_removed: false,
        });
        match e.verdict(&forged) {
            CandidateVerdict::Reject(Some(op)) => {
                assert_eq!(op.member_id, b"bob", "the signer, not the wire claim");
                assert_eq!(op.event, ScoreEvent::BrokenCommit);
            }
            _ => panic!("a forged steward id must be rejected"),
        }
    }

    /// A commit carrying proposal kinds the engine cannot read has no
    /// checkable action set.
    #[test]
    fn a_commit_with_unreadable_proposals_is_rejected() {
        let mut e = engine(&["alice", "bob"]);
        e.queues.insert_approved_proposal(7, invite("carol"));
        let mut opaque = remote("bob", 2, b"commit");
        opaque.facts = Some(StagedFacts {
            sender: id("bob"),
            epoch: 0,
            actions: vec![Action::Add {
                member: id("carol"),
                key_package: b"kp:carol".to_vec(),
            }],
            proposal_count: 2,
            self_removed: false,
        });
        match e.verdict(&opaque) {
            CandidateVerdict::Reject(Some(op)) => {
                assert_eq!(op.event, ScoreEvent::BrokenMlsProposal)
            }
            _ => panic!("a commit the round cannot read must be rejected"),
        }
    }

    /// The actions must be exactly the voted set, keyed the same on both
    /// sides.
    #[test]
    fn actions_beyond_the_voted_set_are_rejected() {
        let mut e = engine(&["alice", "bob"]);
        e.queues.insert_approved_proposal(7, invite("carol"));
        let mut extra = remote("bob", 2, b"commit");
        extra.envelope.proposal_count = 2;
        extra.facts = Some(StagedFacts {
            sender: id("bob"),
            epoch: 0,
            actions: vec![
                Action::Add {
                    member: id("carol"),
                    key_package: b"kp:carol".to_vec(),
                },
                Action::Remove {
                    member: id("alice"),
                },
            ],
            proposal_count: 2,
            self_removed: true,
        });
        match e.verdict(&extra) {
            CandidateVerdict::Reject(Some(op)) => {
                assert_eq!(op.event, ScoreEvent::BrokenMlsProposal)
            }
            _ => panic!("actions beyond the voted set must be rejected"),
        }
    }

    /// The voted set and the commit are compared as deduplicated sets: two
    /// approvals naming one change are committed once.
    #[test]
    fn duplicate_approvals_of_one_change_match_a_single_action() {
        let mut e = engine(&["alice", "bob"]);
        e.queues.insert_approved_proposal(7, invite("carol"));
        e.queues.insert_approved_proposal(8, invite("carol"));
        assert!(actions_match_voted(
            &e.queues,
            &[Action::Add {
                member: id("carol"),
                key_package: b"kp:carol".to_vec(),
            }]
        ));
    }

    // ── the round end to end ───────────────────────────────────────────

    #[test]
    fn our_own_candidate_wins_and_the_merge_report_moves_the_view() {
        let mut e = engine(&["alice"]);
        e.queues.insert_approved_proposal(7, invite("bob"));
        e.begin(at(0));
        assert!(e.request_own_candidate().unwrap());
        assert_eq!(
            e.pending_build.as_ref().map(|b| b.actions.clone()),
            Some(vec![Action::Add {
                member: id("bob"),
                key_package: b"kp:bob".to_vec(),
            }])
        );

        let hash = CommitHash::of(b"commit");
        let out = e
            .own_candidate_built(at(0), hash, 1, b"commit".to_vec())
            .unwrap();
        assert!(matches!(out.outbound.first(), Some(Outbound::Candidate(_))));

        let winner = e
            .select_epoch_steward_candidate()
            .unwrap()
            .expect("our own candidate wins");
        assert!(winner.is_local && winner.losers.is_empty());
        e.decide_winner(winner);

        let out = e
            .commit_applied(at(0), hash, 1, &[id("alice"), id("bob")])
            .unwrap();
        assert_eq!(e.epoch(), 1);
        assert!(e.members().contains(&id("bob")));
        assert!(
            out.events.iter().any(|ev| matches!(
                ev,
                Event::MembersChanged { added, removed }
                    if added == &vec![id("bob")] && removed.is_empty()
            )),
            "the seated joiner is reported once, got {:?}",
            out.events
        );
        assert_eq!(
            e.queues.approved_proposals_count(),
            0,
            "the committed batch leaves the queue"
        );
        assert!(e.queues.has_committed_hash(&hash));
        assert_eq!(e.commit_candidate_count().0, 0, "the round is over");
    }

    /// A merge report for a commit the engine never decided still moves the
    /// view — the group is the truth — but says so.
    #[test]
    fn an_unexpected_merge_is_adopted_and_reported() {
        let mut e = engine(&["alice", "bob"]);
        let out = e
            .commit_applied(at(0), CommitHash::of(b"surprise"), 4, &[id("alice")])
            .unwrap();
        assert_eq!(e.epoch(), 4);
        assert_eq!(e.members(), vec![id("alice")]);
        assert!(out.events.iter().any(|ev| matches!(
            ev,
            Event::Error { operation, .. } if operation == "commit_applied"
        )));
    }

    /// A commit that unseats us ends the conversation: no steward work, one
    /// `Leave`.
    #[test]
    fn a_commit_that_removes_us_asks_the_router_to_leave() {
        let mut e = engine(&["alice", "bob"]);
        e.queues.insert_approved_proposal(7, removal("alice"));
        let hash = CommitHash::of(b"commit");
        e.pending_merge = Some(PendingMerge {
            hash,
            facts: Some(StagedFacts {
                sender: id("bob"),
                epoch: 0,
                actions: vec![Action::Remove {
                    member: id("alice"),
                }],
                proposal_count: 1,
                self_removed: true,
            }),
            is_local: false,
        });
        let out = e.commit_applied(at(0), hash, 1, &[id("bob")]).unwrap();
        assert_eq!(out.decisions, vec![Decision::Leave]);
    }

    /// A candidate from a non-epoch-steward is rejected and scored
    /// `MisbehavingCommit`, even when its actions exactly match the voted
    /// set — only the epoch steward's own candidate may win a round.
    #[test]
    fn a_non_epoch_steward_candidate_is_rejected_even_with_matching_actions() {
        let mut e = engine(&["alice", "bob", "carol"]);
        e.queues.insert_approved_proposal(7, invite("dave"));
        let es = e.expected_steward().unwrap();
        let impostor = ["alice", "bob", "carol"]
            .into_iter()
            .find(|m| id(m).as_bytes() != es.as_slice())
            .expect("at least one member isn't the epoch steward");
        let action = Action::Add {
            member: id("dave"),
            key_package: b"kp:dave".to_vec(),
        };
        let mut candidate = remote(impostor, 1, b"commit");
        candidate.facts = Some(StagedFacts {
            sender: id(impostor),
            epoch: 0,
            actions: vec![action],
            proposal_count: 1,
            self_removed: false,
        });
        e.round.insert(0, candidate, 3);

        let before = e.scoring.score_for(impostor.as_bytes()).unwrap();
        let winner = e.select_epoch_steward_candidate().unwrap();
        assert!(winner.is_none(), "no epoch-steward candidate was submitted");
        let after = e.scoring.score_for(impostor.as_bytes()).unwrap();
        assert!(after < before, "the impostor is penalised");
    }

    /// A silent epoch steward — no candidate submitted before the round
    /// closes — yields `CommitMissing` naming it, keeps the approved batch
    /// queued, and scores it `CensorshipInactivity`.
    #[test]
    fn a_silent_epoch_steward_yields_commit_missing_and_keeps_the_batch() {
        let mut e = engine(&["alice", "bob"]);
        if e.expected_steward().unwrap() == e.own {
            // The member set is the same either way; swapping which one is
            // `own` moves the deterministic epoch steward off us.
            e = engine(&["bob", "alice"]);
        }
        e.queues.insert_approved_proposal(7, invite("carol"));
        let es = e.expected_steward().unwrap();
        assert_ne!(es, e.own, "the test needs a steward other than us");

        e.begin(at(0));
        let before = e.scoring.score_for(&es).unwrap();
        e.close_round_without_commit().unwrap();
        let out = e.finish();

        assert!(out.events.iter().any(|ev| matches!(
            ev,
            Event::CommitMissing { steward, .. } if steward == &Some(MemberId::from(es.as_slice()))
        )));
        let after = e.scoring.score_for(&es).unwrap();
        assert!(after < before, "the silent steward is penalised");
        assert_eq!(
            e.queues.approved_proposals_count(),
            1,
            "the approved batch stays queued"
        );
        assert_eq!(e.phase(), crate::engine::types::Phase::Working);
    }

    /// A merge the router could not execute closes the round with no commit:
    /// no second merge is asked for, and the approved batch stays queued for
    /// the next round.
    #[test]
    fn a_failed_merge_closes_the_round_without_a_commit() {
        let mut e = engine(&["alice", "bob", "carol"]);
        e.queues.insert_approved_proposal(7, invite("dave"));
        let es = String::from_utf8(e.expected_steward().unwrap()).unwrap();
        let action = Action::Add {
            member: id("dave"),
            key_package: b"kp:dave".to_vec(),
        };
        let commit = format!("commit:{es}").into_bytes();
        let mut candidate = remote(&es, 1, &commit);
        candidate.facts = Some(StagedFacts {
            sender: id(&es),
            epoch: 0,
            actions: vec![action],
            proposal_count: 1,
            self_removed: false,
        });
        let hash = candidate.hash;
        e.round.insert(0, candidate, 3);

        e.begin(at(0));
        e.start_freezing();
        e.start_selection();
        let winner = e
            .select_epoch_steward_candidate()
            .unwrap()
            .expect("the epoch steward's candidate wins");
        assert_eq!(winner.hash, hash);
        e.decide_winner(winner);
        let _ = e.finish();

        let out = e
            .decision_failed(
                at(0),
                DecisionFailure {
                    decision: Decision::Merge { hash },
                    reason: "no such staged commit".to_owned(),
                },
            )
            .unwrap();
        assert!(
            !out.decisions
                .iter()
                .any(|d| matches!(d, Decision::Merge { .. })),
            "no second merge is asked for"
        );
        assert!(e.pending_merge.is_none());
        assert!(out.events.iter().any(|ev| matches!(
            ev,
            Event::CommitMissing { steward, .. } if steward == &Some(id(&es))
        )));
        assert_eq!(e.phase(), crate::engine::types::Phase::Working);
        assert_eq!(
            e.queues.approved_proposals_count(),
            1,
            "the approved batch stays queued for the next round"
        );
    }
}
