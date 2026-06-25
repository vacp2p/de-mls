//! Unified per-cycle polling entry point for [`crate::Conversation`].
//!
//! [`Conversation::poll`] drives all time-based conversation paths in one call:
//! consensus-deadline ticks, freeze progression, and steward-inactivity freeze
//! entry. The sub-steps are private to this module — the integrator calls
//! `poll()` once per wakeup cycle and reacts to the returned [`PollOutcome`].

use std::error::Error as StdError;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use prost::Message;
use tracing::{error, info, warn};

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationEvent, ConversationState,
    DispatchOutcome, FreezeFinalizeResult, FreezeOutcome, PeerScoreStorage, ScoreEvent, ScoreOp,
    protos::de_mls::messages::v1::AppMessage,
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

impl<C, Sc> Conversation<C, Sc>
where
    C: ConsensusPlugin,
    Sc: PeerScoreStorage,
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

        match self.advance_freeze(provider, signer) {
            Ok(DispatchOutcome::LeaveRequested) => leave_requested = true,
            Ok(_) => {}
            Err(e) => warn!(
                conversation = %self.conversation_id,
                error = %e,
                "advance_freeze error in poll"
            ),
        }

        if let Err(e) = self.drive_buffered_proposals(provider, signer) {
            warn!(
                conversation = %self.conversation_id,
                error = %e,
                "buffered-proposal drive error in poll"
            );
        }

        if let Err(e) = self.start_freeze_on_inactivity(provider, signer) {
            warn!(
                conversation = %self.conversation_id,
                error = %e,
                "inactivity-freeze error in poll"
            );
        }

        PollOutcome {
            next_wakeup_in: self.next_wakeup_in(),
            leave_requested,
        }
    }

    /// Drive the freeze phase forward. While `Freezing`, emits
    /// [`ConversationEvent::FreezeProgress`] as candidates arrive; once all
    /// expected candidates are in or the freeze window elapses, transitions
    /// to `Selection`, finalises the round, and dispatches the resulting
    /// [`crate::ProcessResult`]. Returns
    /// [`DispatchOutcome::LeaveRequested`] if the applied commit ejected the
    /// local member — `poll()` surfaces that as `leave_requested`.
    fn advance_freeze<Pr>(
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
            self.timing.last_freeze_progress = None;
            return Ok(DispatchOutcome::Done);
        }

        // Early selection: skip remaining freeze time if all expected
        // stewards have submitted candidates.
        let all_candidates_in = self
            .services
            .steward_list
            .current_list()
            .is_some_and(|list| self.queues.freeze_candidate_count() >= list.len());

        if !all_candidates_in && !self.is_freeze_timed_out() {
            // Still freezing — surface candidate progress when it changes.
            let (received, expected) = self.freeze_candidate_count();
            if self.timing.last_freeze_progress != Some((received, expected)) {
                self.timing.last_freeze_progress = Some((received, expected));
                self.emit_event(ConversationEvent::FreezeProgress { received, expected });
            }
            return Ok(DispatchOutcome::Done);
        }
        self.timing.last_freeze_progress = None;

        let selection_event = self.start_selection();
        let has_proposals = self.queues.approved_proposals_count() > 0;
        self.emit_event(ConversationEvent::PhaseChange(selection_event));

        let conversation_id = self.conversation_id.clone();
        let allow_subset = self.services.steward_list.config().allow_subset_candidates;
        let self_member_id = Arc::clone(&self.self_member_id);
        let mut finalize_result =
            match self.finalize_freeze_round(provider, allow_subset, &self_member_id) {
                Ok(result) => result,
                Err(e) => {
                    error!(conversation = %conversation_id, error = %e, "freeze finalize failed");
                    FreezeFinalizeResult::default()
                }
            };
        // Apply locally-observed score events. These come from dropped
        // candidates in the phase-3 loop (RFC §Peer Scoring: direct local
        // observation, no ECP needed). The removal sweep below picks up any
        // member these ops pushed below threshold.
        let applied_score_ops = !finalize_result.score_ops.is_empty();
        if applied_score_ops {
            self.services
                .scoring
                .apply_ops(&finalize_result.score_ops)?;
        }

        if !finalize_result.committed_batch.is_empty() {
            self.emit_event(ConversationEvent::CommitApplied(std::mem::take(
                &mut finalize_result.committed_batch,
            )));
        }

        // `check_and_initiate_score_removals` re-scans `members_below_threshold`
        // and calls `initiate_proposal` for any uncovered target.
        if applied_score_ops
            && let Err(e) = self.check_and_initiate_score_removals(provider, signer)
        {
            error!(conversation = %conversation_id, error = %e, "score-removal check failed (freeze finalize)");
        }

        match finalize_result.outcome {
            FreezeOutcome::Applied { result, welcome } => {
                if let Some(mut welcome) = welcome {
                    // Bundle ConversationSync (steward list + timing +
                    // scores) into the welcome event so the integrator
                    // delivers both atomically. The joiner replays the
                    // sync payload through `handle_inbound` after MLS
                    // attaches.
                    //
                    // Reconcile the list to the just-merged epoch first, so a
                    // small group's sync carries the regenerated, joiner-
                    // inclusive list rather than the pre-commit one. A large
                    // group leaves the list for the post-commit election.
                    welcome.conversation_sync_bytes = {
                        let _ = self.reconcile_steward_list()?;
                        self.build_conversation_sync_payload(provider, signer)?
                            .unwrap_or_default()
                    };
                    // Broadcast the welcome to the group so every member can deliver it to the
                    // joiners, then surface it locally as freshly minted.
                    let broadcast_payload = AppMessage::from(welcome.clone()).encode_to_vec();
                    self.emit_event(ConversationEvent::WelcomeReady {
                        welcome,
                        minted_locally: true,
                    });
                    self.broadcast(broadcast_payload);
                }

                let outcome = match self.dispatch_inbound_result(provider, result, signer) {
                    Ok(o) => o,
                    Err(e) => {
                        error!(conversation = %conversation_id, error = %e, "finalize result dispatch failed");
                        DispatchOutcome::Done
                    }
                };
                Ok(outcome)
            }
            FreezeOutcome::NoCandidate => {
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
                        self.services.scoring.apply_op(&ScoreOp {
                            member_id: steward_id,
                            event: ScoreEvent::CensorshipInactivity,
                        })?;
                    }

                    (event, accused)
                } else {
                    self.queues.clear_freeze_round();
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

    /// Drive the backup-steward proposal takeover. The epoch steward sponsors
    /// announced joiners immediately; everyone else only buffers them. If the
    /// epoch steward stays silent, the join would stall — so here a backup
    /// steward drains the buffer once the recovery window passes, mirroring the
    /// commit takeover (primary leads, backup follows after `recovery`).
    ///
    /// No-op outside `Working`; deferred while approved work is pending (the
    /// commit path owns that, and the next epoch advance drains the buffer);
    /// the epoch steward drains immediately; plain members never drain.
    fn drive_buffered_proposals<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let idle = self.current_state() != ConversationState::Working
            || self.queues.approved_proposals_count() > 0
            || self.actionable_buffered_updates()?.is_empty();
        if idle {
            self.timing.buffered_propose_anchor = None;
            return Ok(());
        }

        // The epoch steward is responsible now — propose without waiting.
        if self.is_epoch_steward()? {
            self.timing.buffered_propose_anchor = None;
            return self.drain_buffered_updates(provider, signer);
        }
        // Only a steward can take over; a plain member just keeps its backup copy.
        if !self.is_steward() {
            return Ok(());
        }

        // Backup steward: give the epoch steward the recovery window first.
        let anchor = match self.timing.buffered_propose_anchor {
            Some(a) => a,
            None => {
                self.timing.buffered_propose_anchor = Some(Instant::now());
                return Ok(());
            }
        };
        let delay =
            self.config.commit_inactivity_duration + self.config.recovery_inactivity_duration;
        if Instant::now() < anchor + delay {
            return Ok(());
        }
        self.timing.buffered_propose_anchor = None;
        info!(
            conversation = %self.conversation_id,
            "backup steward proposing buffered updates: epoch steward silent past recovery window"
        );
        self.drain_buffered_updates(provider, signer)
    }

    /// Steward-inactivity freeze entry: once the inactivity timer fires with
    /// approved work pending, start the freeze round and transition into
    /// `Freezing`. Stewards build their own commit candidate too;
    /// candidate-build failure is logged and the freeze transition proceeds
    /// (peers' candidates still get processed). No-ops outside `Working`
    /// (via `check_steward_inactivity`) and while an election is in flight.
    fn start_freeze_on_inactivity<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let proposal_count = self.queues.approved_proposals_count();
        // Hold the freeze while an election is in flight — committing on
        // the known-stale list would just produce a NoCandidate.
        if self.queues.has_election_in_flight() {
            return Ok(());
        }
        // Recovery uses the shorter retry inactivity window so we don't
        // burn another full epoch waiting for a steward to commit.
        let in_recovery =
            self.is_in_recovery_mode() || self.services.steward_list.next_election_round() > 0;
        let inactivity = if in_recovery {
            self.config.recovery_inactivity_duration
        } else if self.is_epoch_steward()? {
            // The primary steward leads: it commits at the commit-inactivity
            // deadline.
            self.config.commit_inactivity_duration
        } else {
            // Backups (and non-stewards) wait an extra recovery window. In the
            // normal case the primary's commit candidate pulls them into freeze
            // first, so they never self-drive it; only a silent primary lets
            // this longer deadline fire and a backup step in.
            self.config.commit_inactivity_duration + self.config.recovery_inactivity_duration
        };
        let Some(event) = self.check_steward_inactivity(proposal_count, inactivity) else {
            return Ok(());
        };
        let epoch = self.mls().current_epoch()?;
        self.queues.start_freeze_round(epoch);

        let self_member_id = Arc::clone(&self.self_member_id);
        let outbound = if self.services.steward_list.is_steward(&self_member_id) {
            match self.create_commit_candidate(provider, signer, &self_member_id) {
                Ok(payload) => payload,
                Err(e) => {
                    error!(
                        conversation = %self.conversation_id,
                        error = %e,
                        "commit candidate build failed"
                    );
                    None
                }
            }
        } else {
            None
        };

        info!(
            conversation = %self.conversation_id,
            approved = proposal_count,
            "steward inactivity transition"
        );

        self.emit_event(ConversationEvent::PhaseChange(event));

        if let Some(payload) = outbound {
            self.broadcast(payload);
        }

        Ok(())
    }
}
