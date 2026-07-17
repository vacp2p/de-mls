//! Unified per-cycle polling entry point for [`crate::Conversation`].
//!
//! [`Conversation::poll`] drives all time-based conversation paths in one call:
//! consensus-deadline ticks, freeze progression, and steward-inactivity freeze
//! entry. The sub-steps are private to this module — the integrator calls
//! `poll()` once per wakeup cycle and reacts to the returned [`PollOutcome`].

use std::error::Error as StdError;
use std::{sync::Arc, time::Duration};

use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use prost::Message;
use tracing::{error, info, warn};

use crate::{
    CommitRoundOutcome, ConsensusPlugin, Conversation, ConversationError, ConversationEvent,
    ConversationState, DispatchOutcome, PeerScoreStorage, ScoreEvent, ScoreOp, WallClock,
    protos::de_mls::messages::v1::{AppMessage, MemberWelcome},
    wall_clock::WallClockExt,
};

/// Summary returned by [`Conversation::poll`] after one polling pass.
#[derive(Debug, Clone)]
pub struct PollOutcome {
    /// Earliest deadline still pending after this pass. Forward to an
    /// external scheduler as the next wakeup hint.
    pub next_wakeup_in: Option<Duration>,
    /// `true` if this conversation should be torn down: a commit ejected the
    /// local member. The integrator must remove the registry entry and clean
    /// up the consensus scope before its next polling cycle.
    pub leave_requested: bool,
}

impl<Cp, Sc, Wc> Conversation<Cp, Sc, Wc>
where
    Cp: ConsensusPlugin,
    Sc: PeerScoreStorage,
    Wc: WallClock,
{
    /// Drive one polling cycle: tick consensus deadlines, advance freeze
    /// state, and check steward inactivity.
    ///
    /// Best-effort: each step runs regardless of whether the previous one
    /// failed; step errors are transient (a step that can't act this cycle
    /// retries on the next) and are logged rather than surfaced.
    ///
    /// Returns [`PollOutcome::leave_requested`] when the conversation is ready
    /// to be torn down; the integrator finalizes the leave.
    ///
    /// `signer` is the local member's MLS signer, threaded into the
    /// steward commit-candidate build and any auto-vote casts that fire
    /// this cycle.
    pub fn poll<Pr>(&mut self, provider: &Pr, signer: &impl Signer) -> PollOutcome
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let mut leave_requested = false;

        self.tick_deadlines(provider, signer);

        match self.advance_freezing(provider, signer) {
            Ok(DispatchOutcome::LeaveRequested) => leave_requested = true,
            Ok(_) => {}
            Err(e) => warn!(
                conversation = %self.conversation_id,
                error = %e,
                "advance_freezing error in poll"
            ),
        }

        PollOutcome {
            next_wakeup_in: self.next_wakeup_in(),
            leave_requested,
        }
    }

    /// Drive the freeze phase forward. While `Freezing`, emits
    /// [`ConversationEvent::CommitRoundProgress`] as candidates arrive; once all
    /// expected candidates are in or the freeze window elapses, transitions
    /// to `Selection`, finalises the round, and dispatches the resulting
    /// [`crate::ProcessResult`]. Returns
    /// [`DispatchOutcome::LeaveRequested`] if the applied commit ejected the
    /// local member — `poll()` surfaces that as `leave_requested`.
    fn advance_freezing<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<DispatchOutcome, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let state = self.current_state();
        if state != ConversationState::Freezing {
            self.timing.last_commit_round_progress = None;
            return Ok(DispatchOutcome::Done);
        }

        let in_recovery = self.is_in_recovery_mode();
        // Layer-3 auto-fallback: once the manual grace passes, every online node
        // mints so a commit lands even if no member explicitly commits.
        if in_recovery {
            self.maybe_auto_commit_recovery(provider, signer);
        }

        let (received, expected) = self.commit_candidate_count();

        // Early selection: skip remaining freeze time if all expected stewards
        // have submitted. Not in recovery — there's no fixed roster to count
        // against (any member may commit), so we wait out the window.
        let all_candidates_in = !in_recovery
            && self
                .services
                .steward_list
                .current_list()
                .is_some_and(|list| received >= list.len());

        // Recovery reuses the shorter `recovery_inactivity_duration` as its
        // collection settle (after the manual grace / auto-mint), rather than
        // the full `freeze_duration`.
        let window_elapsed = if in_recovery {
            let window = self
                .config
                .recovery_auto_commit_delay
                .unwrap_or(Duration::ZERO)
                + self.config.recovery_inactivity_duration;
            self.timing
                .phase_timer
                .elapsed_since_anchor(self.clock.timestamp(), window)
        } else {
            self.is_freeze_window_elapsed()
        };

        if !all_candidates_in && !window_elapsed {
            // Still freezing — surface candidate progress when it changes.
            if self.timing.last_commit_round_progress != Some((received, expected)) {
                self.timing.last_commit_round_progress = Some((received, expected));
                self.emit_event(ConversationEvent::CommitRoundProgress { received, expected });
            }
            return Ok(DispatchOutcome::Done);
        }
        self.timing.last_commit_round_progress = None;

        let selection_event = self.start_selection();
        let has_proposals = self.queues.approved_proposals_count() > 0;
        self.emit_event(ConversationEvent::PhaseChange(selection_event));

        let conversation_id = self.conversation_id.clone();
        // Recovery accepts subset candidates: any member may commit whatever
        // subset of the approved set they hold, and selection picks the best.
        let allow_subset =
            in_recovery || self.services.steward_list.config().allow_subset_candidates;
        let self_member_id = Arc::clone(&self.self_member_id);
        let mut finalize_result = match self.finalize_commit_round(
            provider,
            allow_subset,
            &self_member_id,
        ) {
            Ok(result) => result,
            Err(e) => {
                // A finalize failure is our own local error, not the steward
                // failing to commit; the NoCandidate path below would wrongly
                // penalize the steward and re-elect. Surface it and retry safely.
                error!(conversation = %conversation_id, error = %e, "commit-round finalize failed");
                self.emit_event(ConversationEvent::Error {
                    operation: "commit_round_finalize".to_owned(),
                    message: e.to_string(),
                });
                return Ok(self.recover_from_finalize_error());
            }
        };
        // Penalties for candidates we dropped while ranking. We saw the
        // misbehavior directly, so no consensus round is needed to score it
        // (RFC §Peer Scoring). If a penalty drops someone to the removal
        // threshold, the sweep below (steward-only) files a removal proposal
        // against them.
        //
        // On failure the penalties are lost, so surface it — but keep going,
        // or the state machine never reaches its terminal transition.
        // `score_ops_applied` gates the sweep on whether they actually landed.
        let score_ops_applied = if finalize_result.score_ops.is_empty() {
            false
        } else {
            match self.services.scoring.apply_ops(&finalize_result.score_ops) {
                Ok(()) => true,
                Err(e) => {
                    error!(conversation = %conversation_id, error = %e, "applying commit-round score ops failed");
                    self.emit_event(ConversationEvent::Error {
                        operation: "apply_commit_round_score_ops".to_owned(),
                        message: e.to_string(),
                    });
                    false
                }
            }
        };

        if !finalize_result.committed_batch.is_empty() {
            self.emit_event(ConversationEvent::CommitApplied(std::mem::take(
                &mut finalize_result.committed_batch,
            )));
        }

        // `check_and_initiate_score_removals` re-scans `members_below_threshold`
        // and calls `initiate_proposal` for any uncovered target.
        if score_ops_applied
            && let Err(e) = self.check_and_initiate_score_removals(provider, signer)
        {
            error!(conversation = %conversation_id, error = %e, "score-removal check failed (commit-round finalize)");
        }

        match finalize_result.outcome {
            CommitRoundOutcome::Applied { result, welcome } => {
                // RFC Layer 3: the first valid commit ends recovery.
                if in_recovery {
                    self.exit_recovery_mode();
                }
                if let Some(welcome) = welcome {
                    self.deliver_welcome(provider, signer, welcome);
                }

                // Always drive the terminal transition: the commit merged, so
                // reach Working even if welcome/dispatch hit a snag.
                // `dispatch_inbound_result` owns it (via `on_conversation_updated`);
                // log rather than propagate so a merged commit can't wedge Selection.
                let outcome = match self.dispatch_inbound_result(provider, result, signer) {
                    Ok(o) => o,
                    Err(e) => {
                        error!(conversation = %conversation_id, error = %e, "finalize result dispatch failed");
                        DispatchOutcome::Done
                    }
                };
                Ok(outcome)
            }
            CommitRoundOutcome::NoCandidate => {
                // Recovery: no commit landed this round (manual-only with no
                // press, or everyone offline). Do NOT fall back to re-election;
                // Layer 3 is already past it. Retry the manual+auto cycle until
                // the integrator's stop-line policy (`recovery_max_rounds`) says
                // to give up — `0` retries forever.
                if in_recovery {
                    // Nothing approved to commit: recovery lands stuck work, so
                    // an empty queue has nothing to recover — exit instead of
                    // spinning the retry loop forever.
                    if !has_proposals {
                        self.queues.commit_round.clear();
                        self.exit_recovery_mode();
                        let resumed = self.start_working();
                        self.emit_event(ConversationEvent::PhaseChange(resumed));
                        return Ok(DispatchOutcome::Done);
                    }
                    return Ok(self.retry_or_exhaust_recovery());
                }
                // `accuse_target` is `Some` only when we had approved proposals
                // go unanswered *and* can attribute the miss to a live steward
                // other than ourselves. Self-penalties are skipped — the
                // node that failed to commit observes its own state directly
                // and doesn't need to record a ScoreOp against itself.
                let (transition_event, accused_steward) = if has_proposals {
                    // Approved batch (and in-flight votes) survive so
                    // the recovered steward commits the same proposals
                    // once the next election lands.
                    let event = self.start_reelection();

                    // Local observation → direct peer-score penalty,
                    // no ECP round-trip. Each honest member records
                    // the same event independently; threshold-crossing
                    // removal still goes through SCORE_BELOW_THRESHOLD
                    // consensus in steward.rs.
                    let accuse_target = {
                        let mls = self.mls();
                        let violation_epoch = mls.current_epoch()?;
                        let members = mls.members()?;
                        let self_member_id: &[u8] = &self.self_member_id;
                        let eligible = self.queues.steward_eligibility(&members);
                        self.services
                            .steward_list
                            .epoch_steward(violation_epoch, &eligible)
                            .filter(|id| !id.is_empty() && *id != self_member_id)
                            .map(|id| id.to_vec())
                    };
                    let accused = accuse_target.is_some();
                    if let Some(steward_id) = accuse_target {
                        // Also strip the accused of recovery proposer
                        // authority — an offline steward must not stay the
                        // only member authorized to elect its replacement.
                        self.queues.note_unresponsive_steward(steward_id.clone());
                        self.services.scoring.apply_op(&ScoreOp {
                            member_id: steward_id,
                            event: ScoreEvent::CensorshipInactivity,
                        })?;
                    }

                    (event, accused)
                } else {
                    self.queues.commit_round.clear();
                    let event = self.start_working();
                    (event, false)
                };

                // A recorded censorship penalty may push the steward below
                // threshold; sweep `members_below_threshold` to act on it.
                if accused_steward
                    && let Err(e) = self.check_and_initiate_score_removals(provider, signer)
                {
                    error!(conversation = %conversation_id, error = %e, "score-removal check failed (freeze timeout)");
                }

                let entered_reelection = transition_event == ConversationState::Reelection;
                self.emit_event(ConversationEvent::PhaseChange(transition_event));

                // Layer 2 recovery: regenerate the steward list. Only the
                // responsible proposer's call actually submits.
                if entered_reelection
                    && let Err(e) = self.initiate_steward_election(provider, true, signer)
                {
                    info!(conversation = %conversation_id, error = %e, "recovery election deferred");
                }

                Ok(DispatchOutcome::Done)
            }
        }
    }

    /// Leave the commit round safely after a finalize failure — no commit landed
    /// and no one is at fault. In recovery, count the round and retry-or-exhaust
    /// (same stop-line as a commit-less round); otherwise resume Working.
    fn recover_from_finalize_error(&mut self) -> DispatchOutcome {
        if self.is_in_recovery_mode() {
            self.retry_or_exhaust_recovery()
        } else {
            let resumed = self.start_working();
            self.emit_event(ConversationEvent::PhaseChange(resumed));
            DispatchOutcome::Done
        }
    }

    /// Advance a commit-less recovery round: count it against the stop-line,
    /// exhausting to Working (with `RecoveryExhausted`) at `recovery_max_rounds`,
    /// otherwise re-opening the collection window (Selection → Freezing) to retry.
    fn retry_or_exhaust_recovery(&mut self) -> DispatchOutcome {
        self.recovery_rounds += 1;
        let max = self.config.recovery_max_rounds;
        if max != 0 && self.recovery_rounds >= max {
            self.exit_recovery_mode();
            let resumed = self.start_working();
            self.emit_event(ConversationEvent::RecoveryExhausted);
            self.emit_event(ConversationEvent::PhaseChange(resumed));
        } else if let Some(reopened) = self.reopen_recovery_window() {
            self.emit_event(ConversationEvent::PhaseChange(reopened));
        }
        DispatchOutcome::Done
    }

    /// Layer-3 auto-fallback: once `recovery_auto_commit_delay` has elapsed with
    /// no local candidate (no member has committed yet), mint our own so a commit
    /// can land. No-op when the policy is manual-only (`None`), before the grace,
    /// or once we've already minted. Best-effort — build errors are logged, not
    /// surfaced, so `poll` keeps driving.
    fn maybe_auto_commit_recovery<Pr>(&mut self, provider: &Pr, signer: &impl Signer)
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let Some(delay) = self.config.recovery_auto_commit_delay else {
            return;
        };
        if !self
            .timing
            .phase_timer
            .elapsed_since_anchor(self.clock.timestamp(), delay)
            || self.queues.commit_round.has_local_candidate()
        {
            return;
        }
        if let Err(e) = self.mint_and_broadcast_candidate(provider, signer) {
            error!(
                conversation = %self.conversation_id,
                error = %e,
                "recovery auto-commit failed"
            );
        }
    }

    /// Hand off the joiner welcome after a commit merged: attach the
    /// `ConversationSync`, broadcast for relay, and surface it locally. Never
    /// propagates — the commit merged, so a sync-build failure surfaces a
    /// [`ConversationEvent::Error`] and ships `welcome_bytes` with empty sync
    /// (the joiner attaches to MLS without it).
    fn deliver_welcome<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        mut welcome: MemberWelcome,
    ) where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        // Reconcile to the just-merged epoch first so the sync carries the
        // joiner-inclusive list, then build it.
        let sync = self
            .reconcile_steward_list()
            .and_then(|_| self.build_conversation_sync_payload(provider, signer));
        match sync {
            Ok(sync_bytes) => welcome.conversation_sync_bytes = sync_bytes.unwrap_or_default(),
            Err(e) => {
                error!(conversation = %self.conversation_id, error = %e, "welcome ConversationSync build failed");
                self.emit_event(ConversationEvent::Error {
                    operation: "welcome_conversation_sync".to_owned(),
                    message: e.to_string(),
                });
            }
        }
        let broadcast_payload = AppMessage::from(welcome.clone()).encode_to_vec();
        self.emit_event(ConversationEvent::WelcomeReady {
            welcome,
            minted_locally: true,
        });
        self.broadcast(broadcast_payload);
    }
}
