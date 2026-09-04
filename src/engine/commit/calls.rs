//! The driving calls the router makes over the commit round: the facts a
//! candidate staged — its own, once `Decision::BuildCommit` returns, or a
//! peer's — and the merge report that moves the engine's own view of the
//! group.
//!
//! Nothing here stages, merges or discards anything itself. The router owns
//! the group; the engine names a hash and the router reports back through
//! [`Engine::commit_applied`] or [`Engine::decision_failed`].

use tracing::{debug, info};

use crate::{
    ConversationError, ScoreEvent,
    engine::{
        commit::select::penalty,
        handle::Engine,
        store::EngineStore,
        types::{
            CommitHash, Decision, DecisionFailure, Event, MemberId, MembershipDelta, Output, Phase,
            StagedFacts, Timestamp,
        },
    },
};

impl<St: EngineStore> Engine<St> {
    /// The facts the router learned by staging a candidate's commit. The
    /// engine keeps at most one per round — the epoch steward's — and
    /// answers anyone else's at once with `Decision::Discard` and a
    /// `MisbehavingCommit` score. Validation against the voted set happens
    /// when the round's window closes.
    pub fn handle_candidate(
        &mut self,
        now: Timestamp,
        hash: CommitHash,
        facts: StagedFacts,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        if facts.epoch != self.epoch {
            debug!(
                conversation = %self.conversation_id,
                candidate_epoch = facts.epoch,
                epoch = self.epoch,
                "candidate discarded: stale epoch"
            );
            self.decide(Decision::Discard { hashes: vec![hash] });
            return self.finish();
        }

        let expected = self.expected_steward();
        if expected.as_deref() != Some(facts.sender.as_bytes()) {
            debug!(
                conversation = %self.conversation_id,
                sender = ?facts.sender,
                "candidate discarded: not the epoch steward"
            );
            self.decide(Decision::Discard { hashes: vec![hash] });
            self.apply_score_ops(&[penalty(
                facts.sender.as_bytes(),
                ScoreEvent::MisbehavingCommit,
            )]);
            return self.finish();
        }

        // The epoch steward's commit opens the round for everyone; a
        // rejected one never does.
        if self.phase == Phase::Working {
            self.open_round_on_peer_candidate()?;
        }

        if let Some((existing_hash, existing)) = &self.round_candidate
            && existing.sender == facts.sender
            && *existing_hash != hash
        {
            debug!(
                conversation = %self.conversation_id,
                "candidate discarded: the epoch steward already sent one this round"
            );
            self.decide(Decision::Discard { hashes: vec![hash] });
            return self.finish();
        }

        self.round_candidate = Some((hash, facts));
        self.emit(Event::CommitRoundProgress {
            received: 1,
            expected: 1,
        });
        self.finish()
    }

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
                self.round_candidate = None;
                self.dirty.meta = true;
                return self.finish();
            }
        };

        let delta = MembershipDelta::between(&self.members, &adopted);
        self.members = adopted;
        self.epoch = epoch;
        self.finalize_committed_batch(epoch, &delta);
        self.reconcile_membership_after_commit(&delta)?;

        // A commit that unseats us ends the conversation here: no steward
        // work applies to a group we are no longer in.
        let self_removed = merged.facts.as_ref().is_some_and(|f| f.self_removed)
            || delta.removed.contains(&self.own);
        if self_removed {
            info!(conversation = %self.conversation_id, "commit removed this member");
            self.cancel_all_auto_votes();
            self.round_candidate = None;
            self.decide(Decision::Leave);
            return self.finish();
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
        self.round_candidate = None;

        // Our own commit seated joiners: broadcast the sync they need as an
        // ordinary control message, sealed at the epoch this merge reached.
        if merged.is_local
            && !delta.added.is_empty()
            && let Err(e) = self.share_conversation_sync()
        {
            self.report_failure("conversation_sync_broadcast", &e);
        }
        self.finish()
    }

    /// Post-commit queue bookkeeping: stamp join epochs and clear the entries
    /// the commit resolved, before the steward-list reconcile reads settled
    /// membership.
    fn finalize_committed_batch(&mut self, epoch: u64, delta: &MembershipDelta) {
        self.queues.apply_membership_delta_bookkeeping(epoch, delta);
        if let Some(target) = self.queues.take_urgent_commit_target() {
            // Urgent commit: the rest of the queue waits for the next cycle.
            self.queues.drop_approved_removals_for(&target);
        } else {
            self.queues.drain_approved_proposals();
        }
        self.dirty.proposals = true;
        self.dirty.join_epochs = true;
        self.dirty.skipped_stewards = true;
        self.dirty.meta = true;
    }

    /// Score each newly seated member from scratch and drop the departed, then
    /// report the change. A leaf whose member changed does both, so the
    /// incoming member starts clean.
    fn reconcile_membership_after_commit(
        &mut self,
        delta: &MembershipDelta,
    ) -> Result<(), ConversationError> {
        for member in &delta.removed {
            self.scoring.remove_member(member);
        }
        for member in &delta.added {
            self.scoring.add_member(member);
        }
        self.dirty.scores = true;
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
            Decision::Merge { .. } => {
                self.round_candidate = None;
                self.pending_merge = None;
                "merge"
            }
            Decision::BuildCommit { .. } => "build_commit",
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
        self.finish()
    }
}
