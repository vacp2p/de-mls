//! Unified per-cycle polling entry point for [`crate::Conversation`].
//!
//! [`Conversation::poll`] drives all time-based conversation paths in one call:
//! consensus-deadline ticks, freeze progression, and steward-inactivity freeze
//! entry. The sub-steps are private to this module — the integrator calls
//! `poll()` once per wakeup cycle, then drains events and outbound.

use std::error::Error as StdError;
use std::time::Duration;

use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use prost::Message;
use tracing::{error, info, warn};

use crate::{
    CommitRoundOutcome, ConsensusPlugin, Conversation, ConversationError, ConversationState, Info,
    Obligation, PeerScoreStorage, Request, ScoreEvent, ScoreOp, WallClock,
    protos::de_mls::messages::v1::{AppMessage, MemberWelcome},
    wall_clock::WallClockExt,
};

impl<Cp, Sc, Wc> Conversation<Cp, Sc, Wc>
where
    Cp: ConsensusPlugin,
    Sc: PeerScoreStorage,
    Wc: WallClock,
{
    /// Drive one polling cycle: tick consensus deadlines, advance freeze
    /// state, and check steward inactivity.
    ///
    /// Every step runs even if an earlier one failed, so there is no single
    /// error to return. Each failure arrives as [`crate::Info::Error`] naming
    /// its step.
    ///
    /// Drain what the cycle produced with [`Conversation::drain_events`] and
    /// [`Conversation::drain_outbound`]. Read [`Conversation::next_wakeup_in`]
    /// last — local actions arm deadlines too.
    ///
    /// `signer` is the local member's MLS signer, threaded into the
    /// steward commit-candidate build and any auto-vote casts that fire
    /// this cycle.
    pub fn poll<Pr>(&mut self, provider: &Pr, signer: &impl Signer)
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.tick_deadlines(provider, signer);

        let result = self.advance_freezing(provider, signer);
        self.report_step("advance_freezing", result);

        let result = self.drive_buffered_proposals(provider, signer);
        self.report_step("drive_buffered_proposals", result);

        let result = self.drive_sync_resend(provider, signer);
        self.report_step("drive_sync_resend", result);

        let result = self.drive_sync_request(provider, signer);
        self.report_step("drive_sync_request", result);

        let result = self.start_freeze_on_inactivity(provider, signer);
        self.report_step("start_freeze_on_inactivity", result);

        let result = self.drive_reelection_retry(provider, signer);
        self.report_step("drive_reelection_retry", result);
    }

    /// Log a failed polling step and report it to the integrator.
    fn report_step(&self, operation: &str, result: Result<(), ConversationError>) {
        let Err(e) = result else { return };
        warn!(
            conversation = %self.conversation_id,
            operation,
            error = %e,
            "polling step failed"
        );
        self.report_failure(operation, &e);
    }

    /// Drive the freeze phase forward. While `Freezing`, emits
    /// [`crate::Info::CommitRoundProgress`] as candidates arrive; once all
    /// expected candidates are in or the freeze window elapses, transitions
    /// to `Selection`, finalises the round, and dispatches the result. A commit
    /// that ejected the local member surfaces as [`crate::Obligation::Left`].
    fn advance_freezing<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let state = self.current_state();
        if state != ConversationState::Freezing {
            self.timing.last_commit_round_progress = None;
            return Ok(());
        }

        let in_recovery = self.is_in_recovery_mode();
        // Layer-3 auto-fallback: once the manual grace passes, every online node
        // mints so a commit lands even if no member explicitly commits.
        if in_recovery {
            self.maybe_auto_commit_recovery(provider, signer);
        }

        let (received, expected) = self.commit_candidate_count();

        // Early selection: skip remaining freeze time if all expected stewards
        // have submitted. Not in recovery — there's no fixed member set to count
        // against (any member may commit), so we wait out the window.
        let all_candidates_in = !in_recovery
            && self
                .services
                .steward_list
                .current_list()
                .is_some_and(|list| received >= list.len());

        // Recovery reuses the shorter `retry_window` as its
        // collection settle (after the manual grace / auto-mint), rather than
        // the full `freeze_duration`.
        let window_elapsed = if in_recovery {
            let window = self
                .config
                .recovery_auto_commit_delay
                .unwrap_or(Duration::ZERO)
                + self.config.retry_window;
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
                self.emit_event(Info::CommitRoundProgress { received, expected });
            }
            return Ok(());
        }
        self.timing.last_commit_round_progress = None;

        let selection_event = self.start_selection();
        let has_proposals = self.queues.approved_proposals_count() > 0;
        self.emit_event(Info::PhaseChange(selection_event));

        let conversation_id = self.conversation_id.clone();
        // Recovery accepts subset candidates: any member may commit whatever
        // subset of the approved set they hold, and selection picks the best.
        let allow_subset =
            in_recovery || self.services.steward_list.config().allow_subset_candidates;
        let self_member_id = self.self_member_id;
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
                self.emit_event(Info::Error {
                    operation: "commit_round_finalize".to_owned(),
                    message: e.to_string(),
                });
                self.recover_from_finalize_error();
                return Ok(());
            }
        };
        // Penalties for misbehavior we saw ourselves, so no group vote.
        // The commit has already merged: propagating would skip the terminal
        // transition below and wedge the round in `Selection`. Report and
        // carry on — no protocol path reads peer scores.
        if !finalize_result.score_ops.is_empty()
            && let Err(e) = self.apply_score_ops(&finalize_result.score_ops)
        {
            error!(conversation = %conversation_id, error = %e, "applying commit-round score ops failed");
            self.emit_event(Info::Error {
                operation: "apply_commit_round_score_ops".to_owned(),
                message: e.to_string(),
            });
        }

        if !finalize_result.committed_batch.is_empty() {
            self.emit_event(Info::CommitApplied(std::mem::take(
                &mut finalize_result.committed_batch,
            )));
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
                if let Err(e) = self.dispatch_inbound_result(provider, result, signer) {
                    error!(conversation = %conversation_id, error = %e, "finalize result dispatch failed");
                }
                Ok(())
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
                        self.emit_event(Info::PhaseChange(resumed));
                        return Ok(());
                    }
                    self.retry_or_exhaust_recovery();
                    return Ok(());
                }
                // `score_target` is `Some` only when approved proposals went
                // unanswered and the miss attributes to a live steward other
                // than ourselves. Self-penalties are skipped — the node that
                // failed to commit sees its own state and needn't score itself.
                let transition_event = if has_proposals {
                    // Approved batch (and in-flight votes) survive so
                    // the recovered steward commits the same proposals
                    // once the next election lands.
                    let event = self.start_reelection();

                    // Local observation → direct peer-score penalty, recorded
                    // the same by every honest member.
                    let score_target = {
                        let mls = self.mls();
                        let violation_epoch = mls.group().epoch().as_u64();
                        let members = mls.members()?;
                        let self_member_id: &[u8] = &self.self_member_id;
                        let eligible = self.queues.steward_eligibility(&members);
                        self.services
                            .steward_list
                            .epoch_steward(violation_epoch, &eligible)
                            .filter(|id| !id.is_empty() && *id != self_member_id)
                            .map(|id| id.to_vec())
                    };
                    if let Some(steward_id) = score_target {
                        self.apply_score_ops(&[ScoreOp {
                            member_id: steward_id,
                            event: ScoreEvent::CensorshipInactivity,
                        }])?;
                    }

                    event
                } else {
                    self.queues.commit_round.clear();
                    self.start_working()
                };

                let entered_reelection = transition_event == ConversationState::Reelection;
                self.emit_event(Info::PhaseChange(transition_event));

                // Layer 2 recovery: regenerate the steward list. Only the
                // responsible proposer's call actually submits.
                if entered_reelection
                    && let Err(e) = self.initiate_steward_election(provider, true, signer)
                {
                    info!(conversation = %conversation_id, error = %e, "recovery election deferred");
                }

                Ok(())
            }
        }
    }

    /// Leave the commit round safely after a finalize failure — no commit landed
    /// and no one is at fault. In recovery, count the round and retry-or-exhaust
    /// (same stop-line as a commit-less round); otherwise resume Working.
    fn recover_from_finalize_error(&mut self) {
        if self.is_in_recovery_mode() {
            self.retry_or_exhaust_recovery()
        } else {
            let resumed = self.start_working();
            self.emit_event(Info::PhaseChange(resumed));
        }
    }

    /// Advance a commit-less recovery round: count it against the stop-line,
    /// exhausting to Working (with `RecoveryExhausted`) at `recovery_max_rounds`,
    /// otherwise re-opening the collection window (Selection → Freezing) to retry.
    fn retry_or_exhaust_recovery(&mut self) {
        self.recovery_rounds += 1;
        let max = self.config.recovery_max_rounds;
        if max != 0 && self.recovery_rounds >= max {
            self.exit_recovery_mode();
            let resumed = self.start_working();
            self.emit_event(Request::RecoveryExhausted);
            self.emit_event(Info::PhaseChange(resumed));
        } else if let Some(reopened) = self.reopen_recovery_window() {
            self.emit_event(Info::PhaseChange(reopened));
        }
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
    /// [`crate::Info::Error`] and ships `welcome_bytes` with empty sync
    /// (the joiner attaches to MLS without it).
    pub(crate) fn deliver_welcome<Pr>(
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
                self.emit_event(Info::Error {
                    operation: "welcome_conversation_sync".to_owned(),
                    message: e.to_string(),
                });
            }
        }
        let broadcast_payload = AppMessage::from(welcome.clone()).encode_to_vec();
        self.emit_event(Obligation::WelcomeReady {
            welcome,
            minted_locally: true,
        });
        self.broadcast(broadcast_payload);
    }

    /// Drive the backup-steward proposal takeover. The epoch steward sponsors
    /// announced joiners immediately; everyone else only buffers them. If the
    /// epoch steward stays silent, the join would stall — so here a backup
    /// steward drains the buffer once `backup_takeover_window` passes, mirroring the
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

        // Backup steward: give the epoch steward `backup_takeover_window` first.
        let now = self.clock.timestamp();
        let anchor = match self.timing.buffered_propose_anchor {
            Some(a) => a,
            None => {
                self.timing.buffered_propose_anchor = Some(now);
                return Ok(());
            }
        };
        if now < anchor + self.config.backup_takeover_window {
            return Ok(());
        }
        self.timing.buffered_propose_anchor = None;
        info!(
            conversation = %self.conversation_id,
            "backup steward proposing buffered updates: epoch steward silent past backup_takeover_window"
        );
        self.drain_buffered_updates(provider, signer)
    }

    /// Backup-steward sync re-send takeover. The epoch steward answers a
    /// `ConversationSyncRequest` reactively; a backup arms `sync_resend_anchor`
    /// and re-sends here only if no `ConversationSync` was observed within
    /// `backup_takeover_window` — so an offline epoch steward can't strand a bootstrap-
    /// less joiner. Only a backup steward drives it; the anchor clears otherwise.
    fn drive_sync_resend<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let Some(anchor) = self.timing.sync_resend_anchor else {
            return Ok(());
        };
        // Only a backup takes over: the epoch steward already answered, and a
        // plain member can't. Either way the anchor no longer applies.
        if self.is_epoch_steward()? || !self.is_steward() {
            self.timing.sync_resend_anchor = None;
            return Ok(());
        }
        if self.clock.timestamp() < anchor + self.config.backup_takeover_window {
            return Ok(());
        }
        self.timing.sync_resend_anchor = None;
        info!(
            conversation = %self.conversation_id,
            "backup steward re-sending conversation sync: epoch steward silent past backup_takeover_window"
        );
        self.share_conversation_sync(provider, signer)
    }

    /// Pull side of sync recovery: when the local steward list is missing or
    /// exhausted at the current epoch and no election is being voted on locally,
    /// ask for the current list. Covers anyone that missed an election — a joiner
    /// welcomed that epoch, or a steward returning from offline whose stale list
    /// still names it. Rate-limited to one request per `backup_takeover_window`
    /// (the answer latency — a backup steward covers a silent epoch steward only
    /// after that window). Skipped while an election is in flight, which resolves
    /// into the list directly.
    fn drive_sync_request<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let current_epoch = self.mls().group().epoch().as_u64();
        let needs_sync = match self.services.steward_list.current_list() {
            None => true,
            Some(_) => self.services.steward_list.is_exhausted(current_epoch),
        };
        if !needs_sync || self.queues.has_election_in_flight() {
            self.timing.sync_request_anchor = None;
            self.timing.unanswered_sync_rounds = 0;
            return Ok(());
        }
        let now = self.clock.timestamp();
        if let Some(anchor) = self.timing.sync_request_anchor
            && now < anchor + self.config.backup_takeover_window
        {
            return Ok(());
        }
        self.timing.sync_request_anchor = Some(now);
        self.timing.unanswered_sync_rounds += 1;
        let alert_at = self.config.unanswered_sync_rounds;
        if alert_at != 0 && self.timing.unanswered_sync_rounds == alert_at {
            self.emit_event(Request::ConversationSyncUnanswered);
        }
        info!(
            conversation = %self.conversation_id,
            rounds = self.timing.unanswered_sync_rounds,
            "requesting conversation sync: local steward list missing or exhausted"
        );
        self.request_conversation_sync(provider, signer)
    }

    /// Steward-inactivity freeze entry: once the inactivity timer fires with
    /// approved work pending, start the commit round and transition into
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
        // burn another full epoch waiting for a steward to commit. That
        // covers open recovery mode, a bumped retry round, and the stretch
        // between a recovery election landing and its commit merging.
        let in_recovery = self.is_in_recovery_mode()
            || self.services.steward_list.next_election_round() > 0
            || self.timing.reelection_recovered;
        let inactivity = if in_recovery {
            self.config.retry_window
        } else {
            self.config.commit_batch_window
        };
        let Some(event) = self.check_steward_inactivity(proposal_count, inactivity) else {
            return Ok(());
        };

        info!(
            conversation = %self.conversation_id,
            approved = proposal_count,
            "steward inactivity transition"
        );
        self.on_freeze_entered(provider, signer, event)?;
        Ok(())
    }

    /// Reelection-silence watchdog: while parked in `Reelection` with no
    /// election in flight, a full silent window counts as a rejected round —
    /// [`Conversation::advance_election_retry`] bumps the retry ladder, which
    /// hands proposer authority to the next candidate and, once retries are
    /// exhausted, escalates to the `Deadlock` emergency proposal. Without
    /// this, a responsible proposer that never submits (e.g. an offline sole
    /// steward) would park the conversation in `Reelection` forever.
    fn drive_reelection_retry<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if self.current_state() != ConversationState::Reelection
            || self.queues.has_election_in_flight()
        {
            self.timing.reelection_silence_anchor = None;
            return Ok(());
        }
        let now = self.clock.timestamp();
        let anchor = match self.timing.reelection_silence_anchor {
            Some(a) => a,
            None => {
                self.timing.reelection_silence_anchor = Some(now);
                return Ok(());
            }
        };
        if now < anchor + self.config.retry_window {
            return Ok(());
        }
        self.timing.reelection_silence_anchor = None;
        info!(
            conversation = %self.conversation_id,
            "no election proposal observed within the reelection window: counting a rejected round"
        );
        self.advance_election_retry(provider, signer)
    }
}
