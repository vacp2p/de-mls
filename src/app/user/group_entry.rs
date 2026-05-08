//! Per-group app-side aggregate: protocol state, MLS service, plug-ins,
//! state machine, phase timer, and group-level config + operating-mode
//! flag. Coordinator methods compose state-machine transitions with
//! phase-timer anchors so callers don't have to manage them in pairs.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use prost::Message;
use tokio::task::JoinHandle;
use tracing::info;

use crate::{
    app::{GroupConfig, PhaseTimer},
    core::{
        BufferedCommitCandidate, CoreError, FreezeFinalizeResult, Group, GroupState,
        GroupStateMachine, OperatingMode, PeerScoringPlugin, ProcessResult, ProposalKind,
        StewardListPlugin, compute_commit_hash, finalize_freeze_round, member_set, process_inbound,
    },
    ds::{APP_MSG_SUBTOPIC, OutboundPacket},
    mls_crypto::{
        CommitCandidate as MlsCommitCandidate, KeyPackageBytes, MlsCommitInput, MlsService,
    },
    protos::de_mls::messages::v1::{AppMessage, CommitCandidate, group_update_request::Payload},
};

/// Per-group auto-vote timer registry. Spawned when a proposal first
/// becomes visible locally (own submit or peer inbound); cancelled on
/// manual vote, consensus resolution, or group leave.
pub(crate) type AutoVoteTimers = Arc<Mutex<HashMap<u32, JoinHandle<()>>>>;

pub(crate) struct GroupEntry<M: MlsService, Sc: PeerScoringPlugin, St: StewardListPlugin> {
    pub(crate) group: Group,
    /// Per-group MLS service. `None` for joiners in `PendingJoin` who
    /// haven't accepted a welcome yet; once attached via
    /// [`Self::attach_mls`] it stays `Some` for the entry's lifetime.
    mls: Option<M>,
    /// Per-group state machine. Coordinator methods on this entry update
    /// it together with `phase_timer` so the two never drift.
    state_machine: GroupStateMachine,
    /// Wall-clock anchor + phase-anchor durations combined with
    /// `state_machine` by the entry's coordinator methods.
    phase_timer: PhaseTimer,
    /// Per-group durable config: voting/consensus durations,
    /// `liveness_criteria_yes`, `pending_update_max_epochs`. Read by
    /// app-layer coordinators; joiner-sync writes through this directly.
    pub(crate) config: GroupConfig,
    /// Per-group peer-score plug-in. Lives next to `state_machine` as
    /// app-layer wiring; protocol decisions read it via the entry's
    /// `RwLock`, no separate `Mutex` needed.
    pub(crate) scoring: Sc,
    /// Per-group steward list plug-in. Holds the active list, retry
    /// counter, and election retry policy. Coordinator composes
    /// eligibility from MLS members + `Group::is_pending_removal` and
    /// passes it on every position query.
    pub(crate) steward: St,
    /// Authorization mode (RFC §Layer 3 Anti-Deadlock ECP). `Recovery` is
    /// set when an accepted Deadlock ECP relaxes the steward gate so any
    /// member may produce the next commit; cleared on accepted election.
    /// Read by the freeze coordinator, the create-commit path, and
    /// `core::finalize_freeze_round` (via `in_recovery` parameter).
    operating_mode: OperatingMode,
    /// Per-proposal auto-vote timers. The spawned task holds a clone of
    /// this `Arc` so it can self-clean on completion; coordinators use
    /// `cancel_auto_vote` / `cancel_all_auto_votes` to abort early.
    pub(crate) auto_vote_timers: AutoVoteTimers,
}

impl<M: MlsService, Sc: PeerScoringPlugin, St: StewardListPlugin> GroupEntry<M, Sc, St> {
    /// Build a fresh entry. Creator path passes `Some(mls)`; joiner
    /// path passes `None` and attaches later via [`Self::attach_mls`].
    pub(crate) fn new(
        group: Group,
        mls: Option<M>,
        state_machine: GroupStateMachine,
        phase_timer: PhaseTimer,
        config: GroupConfig,
        scoring: Sc,
        steward: St,
    ) -> Self {
        Self {
            group,
            mls,
            state_machine,
            phase_timer,
            config,
            scoring,
            steward,
            operating_mode: OperatingMode::Normal,
            auto_vote_timers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // ── Auto-vote timers ────────────────────────────────────────────

    /// Abort the auto-vote timer for `proposal_id` if one is registered.
    /// No-op otherwise.
    pub(crate) fn cancel_auto_vote(&self, proposal_id: u32) {
        if let Ok(mut timers) = self.auto_vote_timers.lock()
            && let Some(handle) = timers.remove(&proposal_id)
        {
            handle.abort();
        }
    }

    /// Abort every auto-vote timer registered on this entry. Called on
    /// group leave so no stale timers fire against a group we've left.
    pub(crate) fn cancel_all_auto_votes(&self) {
        if let Ok(mut timers) = self.auto_vote_timers.lock() {
            for (_, handle) in timers.drain() {
                handle.abort();
            }
        }
    }

    // ── Operating mode (Layer 3 Anti-Deadlock) ──────────────────────

    pub(crate) fn is_in_recovery_mode(&self) -> bool {
        self.operating_mode == OperatingMode::Recovery
    }

    pub(crate) fn enter_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Recovery;
    }

    pub(crate) fn exit_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Normal;
    }

    // ── State-machine + phase-timer coordinators ────────────────────

    pub(crate) fn current_state(&self) -> GroupState {
        self.state_machine.current_state()
    }

    pub(crate) fn start_working(&mut self) -> GroupState {
        self.state_machine.start_working();
        self.phase_timer.clear();
        info!(state = "Working", "state transition");
        GroupState::Working
    }

    pub(crate) fn start_freezing(&mut self) -> GroupState {
        self.state_machine.start_freezing();
        self.phase_timer.start();
        info!(state = "Freezing", "state transition");
        GroupState::Freezing
    }

    /// Bypass the inactivity timer and enter Freezing immediately. Returns
    /// `Some(Freezing)` on transition (only fires from Working or
    /// Reelection); `None` from other states.
    pub(crate) fn force_freezing(&mut self) -> Option<GroupState> {
        if self.state_machine.force_freezing() {
            self.phase_timer.start();
            info!(state = "Freezing", "state transition (forced)");
            Some(GroupState::Freezing)
        } else {
            None
        }
    }

    pub(crate) fn start_selection(&mut self) -> GroupState {
        self.state_machine.start_selection();
        info!(state = "Selection", "state transition");
        GroupState::Selection
    }

    pub(crate) fn start_reelection(&mut self) -> GroupState {
        self.state_machine.start_reelection();
        self.phase_timer.clear();
        info!(state = "Reelection", "state transition");
        GroupState::Reelection
    }

    /// `true` once 3× `commit_inactivity_duration` has passed in
    /// `PendingJoin` without a welcome.
    ///
    /// Pipeline: consensus (~15s) + commit_inactivity + freeze (≈ commit/2)
    /// = ~1.5× commit-inactivity + consensus overhead. Use 3× for safety margin.
    pub(crate) fn is_pending_join_expired(&self) -> bool {
        self.state_machine.current_state() == GroupState::PendingJoin
            && self
                .phase_timer
                .elapsed_since_anchor(self.config.commit_inactivity_duration * 3)
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub(crate) fn is_freeze_timed_out(&self) -> bool {
        self.state_machine.current_state() == GroupState::Freezing
            && self
                .phase_timer
                .elapsed_since_anchor(self.config.freeze_duration)
    }

    /// Drives the "steward waited too long to commit" transition into
    /// `Freezing`. Call each poll tick. Returns `Some(Freezing)` exactly
    /// on the tick that transitions; `None` while still waiting, outside
    /// Working, or when there's no approved work. Self-starts the
    /// inactivity anchor on the first tick with approved work.
    pub(crate) fn check_steward_inactivity(
        &mut self,
        approved_proposals_count: usize,
        inactivity_duration: Duration,
    ) -> Option<GroupState> {
        if self.state_machine.current_state() != GroupState::Working
            || approved_proposals_count == 0
        {
            return None;
        }
        if self.phase_timer.started_at().is_none() {
            self.phase_timer.start();
            info!(
                approved = approved_proposals_count,
                inactivity_ms = inactivity_duration.as_millis() as u64,
                "inactivity timer started"
            );
            return None;
        }
        if !self.phase_timer.elapsed_since_anchor(inactivity_duration) {
            return None;
        }
        info!(
            inactivity_ms = inactivity_duration.as_millis() as u64,
            approved = approved_proposals_count,
            "inactivity window elapsed, entering freeze"
        );
        Some(self.start_freezing())
    }

    /// Borrow the MLS service, if attached. `None` for joiners
    /// pre-welcome.
    pub(crate) fn mls(&self) -> Option<&M> {
        self.mls.as_ref()
    }

    /// Borrow the MLS service, erroring with
    /// [`crate::core::CoreError::MlsGroupNotInitialized`] when not
    /// attached. Use this in code paths where the service must be
    /// present so the `?` chain stays linear.
    pub(crate) fn expect_mls(&self) -> Result<&M, CoreError> {
        self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)
    }

    /// Attach an MLS service. Called by joiners after the welcome
    /// arrives. Caller is responsible for not double-attaching.
    pub(crate) fn attach_mls(&mut self, mls: M) {
        self.mls = Some(mls);
    }

    /// Drop the attached MLS service and return it. Used on group leave
    /// so the caller can run service-side cleanup (`mls.delete()`).
    pub(crate) fn take_mls(&mut self) -> Option<M> {
        self.mls.take()
    }

    // ── Protocol-function wrappers ─────────────────────────────────
    //
    // Pull `group`, `mls`, and `steward` from `self` so coordinator
    // callsites don't destructure the entry. Protocol logic stays in
    // `core::api`; these are pure delegation.

    /// Current MLS members; empty when no service is attached
    /// (joiner pre-welcome).
    pub(crate) fn group_members(&self) -> Result<Vec<Vec<u8>>, CoreError> {
        match &self.mls {
            Some(mls) => Ok(mls.members()?),
            None => Err(CoreError::MlsGroupNotInitialized),
        }
    }

    /// Build a commit candidate. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached.
    pub(crate) fn create_commit_candidate(
        &mut self,
        self_identity: &[u8],
        app_id: &[u8],
    ) -> Result<Option<OutboundPacket>, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        if !self.steward.is_steward(self_identity) && !self.is_in_recovery_mode() {
            return Err(CoreError::NotASteward);
        }

        if self.group.approved_proposals().is_empty() {
            return Err(CoreError::NoProposals);
        }

        // MLS forbids committing one's own removal. If the approved batch contains
        // RemoveMember(self), skip local candidate creation — another steward will
        // commit the batch (including this node's removal) once they enter freeze.
        let self_removal_pending = self.group.approved_proposals().values().any(|req| {
            matches!(
                req.payload.as_ref(),
                Some(Payload::RemoveMember(r))
                    if r.identity == self_identity
            )
        });
        if self_removal_pending {
            info!(
                group = self.group.group_name(),
                "commit candidate skipped: approved batch contains self-remove"
            );
            return Ok(None);
        }

        // Governance proposals (emergency, election) are consensus-only and must
        // not be in the approved queue at batch creation time.
        let non_mls_ids: Vec<u32> = self
            .group
            .approved_proposals()
            .iter()
            .filter(|(_, req)| ProposalKind::of(req).is_governance())
            .map(|(&id, _)| id)
            .collect();

        if !non_mls_ids.is_empty() {
            return Err(CoreError::UnexpectedNonMlsProposals {
                proposal_ids: non_mls_ids,
            });
        }

        // Drop approved entries already reflected in group state (stale
        // rebroadcast KPs, duplicate removes) — without this MLS would reject
        // the whole batch with "Duplicate signature key in proposals and group".
        let current_members = mls.members()?;
        let current_members_set = member_set(&current_members);
        let is_member = |id: &[u8]| current_members_set.contains(id);

        // Urgent (ECP-driven) freeze: restrict the batch to just the target's
        // RemoveMember. See `Group::urgent_commit_target`.
        let urgent_target = self.group.urgent_commit_target().map(|t| t.to_vec());

        // Iterate in insertion order (FIFO): library proposal IDs are
        // content-derived hashes, so sort-by-id is not temporal.
        let k_max = mls.commit_batch_max();
        let mut updates = Vec::with_capacity(self.group.approved_order().len().min(k_max));
        for pid in self.group.approved_order() {
            if updates.len() >= k_max {
                break;
            }
            let Some(proposal) = self.group.approved_proposals().get(pid) else {
                continue;
            };
            match proposal.payload.as_ref() {
                Some(Payload::InviteMember(im)) => {
                    if urgent_target.is_some() {
                        continue;
                    }
                    if is_member(&im.identity) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Add(KeyPackageBytes::new(
                        im.key_package_bytes.clone(),
                        im.identity.clone(),
                    )));
                }
                Some(Payload::RemoveMember(rm)) => {
                    if let Some(target) = urgent_target.as_deref()
                        && rm.identity != target
                    {
                        continue;
                    }
                    if !is_member(&rm.identity) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Remove(rm.identity.clone()));
                }
                _ => return Err(CoreError::InvalidGroupUpdateRequest),
            }
        }

        if updates.is_empty() {
            return Ok(None);
        }

        let MlsCommitCandidate {
            proposals: mls_proposals,
            commit,
            welcome,
        } = mls.create_commit_candidate(&updates)?;

        let candidate = CommitCandidate {
            group_name: self.group.group_name_bytes().to_vec(),
            mls_proposals,
            commit_message: commit,
            steward_identity: self_identity.to_vec(),
        };

        // Welcome bytes are deferred: sent from finalize_freeze_round after the
        // commit merges, so joiners can't advance epoch ahead of the steward.
        let commit_hash = compute_commit_hash(&candidate.commit_message);
        let epoch = mls.current_epoch()?;
        let _ = self.group.add_freeze_candidate(
            BufferedCommitCandidate {
                candidate_msg: candidate.clone(),
                commit_hash,
                is_local_candidate: true,
                welcome_bytes: welcome,
            },
            epoch,
        );

        info!(
            group = self.group.group_name(),
            epoch,
            proposals = updates.len(),
            "commit candidate created"
        );

        let candidate_msg: AppMessage = candidate.into();
        Ok(Some(OutboundPacket::new(
            candidate_msg.encode_to_vec(),
            APP_MSG_SUBTOPIC,
            self.group.group_name(),
            app_id,
        )))
    }

    /// Finalize the active freeze round. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached.
    pub(crate) fn finalize_freeze_round(
        &mut self,
        allow_subset_candidates: bool,
        app_id: &[u8],
    ) -> Result<FreezeFinalizeResult, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        let in_recovery = self.operating_mode == OperatingMode::Recovery;
        finalize_freeze_round(
            &mut self.group,
            mls,
            &self.steward,
            in_recovery,
            allow_subset_candidates,
            app_id,
        )
    }

    /// Process an inbound app-subtopic payload. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached — caller should check `mls().is_some()` first.
    pub(crate) fn process_inbound(&mut self, payload: &[u8]) -> Result<ProcessResult, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        process_inbound(&mut self.group, mls, payload)
    }
}
