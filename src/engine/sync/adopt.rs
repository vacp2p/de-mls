//! Validating and adopting a `ConversationSync`: the recomputation that
//! verifies a steward's answer, and applying the list, timing and peer
//! scores it carries.

use std::collections::HashSet;

use tracing::{info, warn};

use crate::{
    ConversationError, DEFAULT_PEER_SCORE, ScoreSnapshot, ScoringConfig, StewardList,
    StewardListConfig,
    engine::{
        handle::Engine,
        store::EngineStore,
        types::{Decision, Event, Phase},
        util::member_set,
    },
    protos::de_mls::messages::v1::{ConversationSync, TimingConfig},
};

impl<St: EngineStore> Engine<St> {
    /// Adopt a steward's `ConversationSync` when this node holds no steward
    /// list, or one from a strictly older election, in any phase.
    /// Reconstructs the candidate pool (members minus the carried unsettled
    /// set), re-derives the steward list to verify it, then applies list,
    /// protocol flags, timing and peer scores. A valid answer settles the
    /// pending request whether or not it is adopted.
    pub(crate) fn on_conversation_sync(
        &mut self,
        sync: ConversationSync,
    ) -> Result<(), ConversationError> {
        let current_epoch = self.epoch;
        // Every node validates, a current one included: only a valid sync
        // settles anything below.
        if !validate_conversation_sync(&self.conversation_id, &sync, current_epoch, &self.members)?
        {
            return Ok(());
        }
        // A valid answer settles the request, adopted or not: the requester's
        // deadline and a backup steward's pending turn.
        self.timing.sync_deadline = None;
        self.timing.sync_takeover_anchor = None;
        if let Some(mine) = self.steward_list.election_epoch()
            && sync.election_epoch <= mine
        {
            // Current: nothing newer to adopt.
            return Ok(());
        }

        let stewards = sync.steward_members.len();
        self.adopt_conversation_sync(&sync)?;
        self.emit(Event::SyncApplied);
        if let Some(working) = self.leave_syncing() {
            self.emit_phase(Some(working));
        } else {
            self.recheck_round_candidate();
        }
        info!(
            conversation = %self.conversation_id,
            election_epoch = sync.election_epoch,
            stewards,
            scores = sync.peer_scores.len(),
            timing = sync.timing.is_some(),
            "conversation sync applied"
        );
        Ok(())
    }

    /// A newer list may name a different epoch steward; a candidate held
    /// from anyone else is dropped and the round reopens on the right
    /// steward's. `Freezing` is the only phase that holds a candidate:
    /// `Selection` lasts from round close to `commit_applied` inside one
    /// drive loop, which no control message enters. Discarding this
    /// node's own hash is a no-op at the router, whose pending commit
    /// clears before the next merge.
    fn recheck_round_candidate(&mut self) {
        if self.phase != Phase::Freezing {
            return;
        }
        let Some((hash, facts)) = &self.round_candidate else {
            return;
        };
        if self.expected_steward().as_deref() == Some(facts.sender.as_bytes()) {
            return;
        }
        let hash = *hash;
        self.round_candidate = None;
        self.decide(Decision::Discard { hashes: vec![hash] });
        let working = self.start_working();
        self.emit_phase(Some(working));
    }

    /// Apply a validated sync: steward list, protocol bounds, retry ceiling,
    /// peer-score pivot and snapshot, and timing.
    fn adopt_conversation_sync(
        &mut self,
        sync: &ConversationSync,
    ) -> Result<(), ConversationError> {
        let list_config = StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?;

        let sn = sync.steward_members.len();
        self.steward_list.set_config(list_config);
        self.steward_list.install_list(
            sync.election_epoch,
            &sync.steward_members,
            sn,
            sync.retry_round,
        )?;
        self.dirty.steward_list = true;
        // The steward's join epochs become ours: a later election pool is
        // computed from them, and a joiner never saw its own add.
        for entry in &sync.join_epochs {
            self.queues
                .record_join_epoch(entry.member_id.clone(), entry.epoch);
        }
        self.dirty.join_epochs = true;
        // Before the snapshot: it rebases members off our assumed default onto
        // the group's, which is exactly the set the sparse `peer_scores` omits.
        self.scoring
            .set_default_score(synced_default_peer_score(sync));
        let snapshot = ScoreSnapshot {
            diverged: sync
                .peer_scores
                .iter()
                .map(|ps| (ps.member_id.clone(), ps.score))
                .collect(),
        };
        // Record the synced scores, reporting how far each member had
        // already diverged from the default this node would have assumed.
        self.apply_score_snapshot(&snapshot);
        self.config.liveness_criteria_yes = sync.liveness_criteria_yes;
        if let Some(timing) = &sync.timing {
            self.config.apply_timing(timing);
        }
        // `voting_delay` is this node's own; the adopted timeout may sit below
        // it, and an auto-vote after the session closes counts as silence.
        if self.config.voting_delay >= self.config.consensus_timeout {
            let clamped = self.config.consensus_timeout / 2;
            warn!(
                conversation = %self.conversation_id,
                voting_delay = ?self.config.voting_delay,
                consensus_timeout = ?self.config.consensus_timeout,
                clamped = ?clamped,
                "voting_delay clamped below the adopted consensus_timeout"
            );
            self.config.voting_delay = clamped;
        }
        self.dirty.meta = true;
        Ok(())
    }
}

/// Returns `true` when the sync is acceptable for application. Logs the
/// rejection reason on `false`.
///
/// `members` is the adopting node's current member set; ghost stewards
/// (removed since the list was elected) are tolerated as long as at least one
/// listed steward is still present.
///
/// The starting score is adopted, not judged: where the group puts it is the
/// integrator's choice. It goes through [`ScoringConfig::validate`] — the
/// same rule a locally built config passes at construction — which asks only
/// that it be positive.
pub(crate) fn validate_conversation_sync(
    conversation_id: &str,
    sync: &ConversationSync,
    current_epoch: u64,
    members: &[Vec<u8>],
) -> Result<bool, ConversationError> {
    if sync.election_epoch > current_epoch {
        info!(
            conversation = conversation_id,
            election_epoch = sync.election_epoch,
            current_epoch,
            "conversation sync rejected: election_epoch > current_epoch"
        );
        return Ok(false);
    }

    let members_set = member_set(members);
    let any_present = sync
        .steward_members
        .iter()
        .any(|s| members_set.contains(s.as_slice()));
    // Only accept join epochs of members that actually exist, so fake ids
    // can't shape the pool. Full protection needs every member to agree on
    // the sync's content.
    if !sync
        .join_epochs
        .iter()
        .all(|j| members_set.contains(j.member_id.as_slice()))
    {
        info!(
            conversation = conversation_id,
            "conversation sync rejected: join epoch of a member not in the current set"
        );
        return Ok(false);
    }
    let unsettled_set: HashSet<&[u8]> = sync
        .join_epochs
        .iter()
        .filter(|j| j.epoch >= sync.election_epoch)
        .map(|j| j.member_id.as_slice())
        .collect();
    let pool: Vec<Vec<u8>> = members
        .iter()
        .filter(|m| !unsettled_set.contains(m.as_slice()))
        .cloned()
        .collect();
    let ordering_valid = StewardList::validate(
        &sync.steward_members,
        sync.election_epoch,
        conversation_id.as_bytes(),
        &pool,
        &StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?,
        sync.retry_round,
    )?;
    if !(any_present && ordering_valid) {
        info!(
            conversation = conversation_id,
            any_present,
            ordering = ordering_valid,
            "conversation sync rejected: invalid"
        );
        return Ok(false);
    }

    if let Some(timing) = &sync.timing
        && let Some(zero_field) = first_zero_timing_field(timing)
    {
        info!(
            conversation = conversation_id,
            field = zero_field,
            "conversation sync rejected: zero-valued timing field"
        );
        return Ok(false);
    }

    // The same `ScoringConfig::validate` a locally built config passes at
    // construction, so the two ends can't drift apart. A zero
    // `default_peer_score` is proto3's "absent" and resolves to the RFC
    // default, so a sender that never set the field is still joinable.
    if let Err(e) = (ScoringConfig {
        default_score: synced_default_peer_score(sync),
    })
    .validate()
    {
        info!(
            conversation = conversation_id,
            error = %e,
            "conversation sync rejected: peer-score config"
        );
        return Ok(false);
    }
    Ok(true)
}

/// `default_peer_score` the sync asks for. proto3 scalars carry no presence,
/// so a zero is indistinguishable from an unset field and means "whatever the
/// program defaults to" — never a literal starting score of zero, which is not
/// a score to start from.
pub(crate) fn synced_default_peer_score(sync: &ConversationSync) -> i64 {
    if sync.default_peer_score == 0 {
        DEFAULT_PEER_SCORE
    } else {
        sync.default_peer_score
    }
}

/// Name of the first zero-valued field in `timing`, or `None` when every
/// field is non-zero. Zero in any timing field would short-circuit the timer
/// it drives (a consensus timeout firing immediately, an inactivity window
/// that never holds).
pub(crate) fn first_zero_timing_field(timing: &TimingConfig) -> Option<&'static str> {
    if timing.commit_batch_window_ms == 0 {
        Some("commit_batch_window_ms")
    } else if timing.freeze_duration_ms == 0 {
        Some("freeze_duration_ms")
    } else if timing.proposal_expiration_ms == 0 {
        Some("proposal_expiration_ms")
    } else if timing.consensus_timeout_ms == 0 {
        Some("consensus_timeout_ms")
    } else if timing.backup_takeover_window_ms == 0 {
        Some("backup_takeover_window_ms")
    } else {
        None
    }
}
