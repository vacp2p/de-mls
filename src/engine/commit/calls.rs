//! The five driving calls the router makes over the commit round: the cheap
//! admission gate before staging, the local mint report, and the merge
//! report that moves the engine's own view of the group.
//!
//! Nothing here stages, merges or discards anything itself. The router owns
//! the group; the engine names a hash and the router reports back through
//! [`Engine::commit_applied`] or [`Engine::decision_failed`].

use prost::Message;
use tracing::{debug, info};

use crate::{
    ConversationError,
    engine::{
        commit::buffer::BufferedCandidate,
        handle::Engine,
        queues::EarlyCandidate,
        store::EngineStore,
        types::{
            Admission, CommitHash, Decision, DecisionFailure, Event, MemberId, MembershipDelta,
            Outbound, Output, Phase, StagedFacts, Timestamp,
        },
    },
    protos::de_mls::messages::v1::CommitCandidate,
};

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

    // ── minting our own candidate ──────────────────────────────────────

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
