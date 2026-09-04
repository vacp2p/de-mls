//! `ConversationSync` bootstrap: building and sharing the sync a bootstrap-
//! less joiner or a degraded member adopts, its validation ladder, and the
//! `ConversationSyncRequest` re-send.

use tracing::{info, warn};

use crate::{
    ConversationError, DEFAULT_PEER_SCORE, ScoreSnapshot, ScoringConfig, StewardList,
    StewardListConfig,
    engine::{handle::Engine, store::EngineStore, types::Event, util::member_set},
    protos::de_mls::messages::v1::{
        ControlMessage, ConversationSync, ConversationSyncRequest, PeerScore, TimingConfig,
        control_message,
    },
};

impl<St: EngineStore> Engine<St> {
    /// The `ConversationSync` control bytes that carry the current steward
    /// list, protocol flags, timing and peer scores. `None` when there is no
    /// list yet; the caller owns delivery.
    pub(crate) fn build_bootstrap(&mut self) -> Result<Option<Vec<u8>>, ConversationError> {
        // Sparse snapshot — only members whose score has diverged from
        // `default_score`, which rides along in `default_peer_score` so the
        // joiner adopts the same pivot and reads the omissions as we meant
        // them. Saves wire size at scale.
        let peer_scores: Vec<PeerScore> = self
            .scoring
            .snapshot()
            .diverged
            .into_iter()
            .map(|(member_id, score)| PeerScore { member_id, score })
            .collect();

        let Some(list) = self.steward_list.current_list() else {
            return Ok(None);
        };
        let steward_members = list.members().to_vec();
        let election_epoch = list.election_epoch();
        let sn_min = list.config().sn_min as u32;
        let sn_max = list.config().sn_max as u32;
        // `retry_round` is the fixed seed that made this list, frozen on the
        // list rather than the next dynamic attempt. Joiners use it to
        // recompute the list order.
        let retry_round = list.retry_round();

        // Members that weren't settled at election time. A joiner uses them to
        // reconstruct the candidate pool and re-derive the list itself.
        let unsettled_members: Vec<Vec<u8>> = self
            .members
            .iter()
            .filter(|m| !self.queues.is_settled(m, election_epoch))
            .cloned()
            .collect();

        let sync = ConversationSync {
            steward_members,
            election_epoch,
            sn_min,
            sn_max,
            peer_scores,
            default_peer_score: self.scoring.default_score(),
            timing: Some(TimingConfig::from(&self.config)),
            retry_round,
            liveness_criteria_yes: self.config.liveness_criteria_yes,
            unsettled_members,
        };
        Ok(Some(control_bytes(
            control_message::Payload::ConversationSync(sync),
        )))
    }

    /// Broadcast the current `ConversationSync` so a bootstrap-less joiner can
    /// adopt it. No-op while there is no list to share.
    pub(crate) fn share_conversation_sync(&mut self) -> Result<(), ConversationError> {
        if let Some(bytes) = self.build_bootstrap()? {
            self.send_control(bytes);
        }
        Ok(())
    }

    /// Broadcast a `ConversationSyncRequest` so a steward re-sends its
    /// `ConversationSync`. The message carries no fields: the router hands the
    /// authenticated sender over, and that is the requester.
    pub(crate) fn broadcast_sync_request(&mut self) {
        let bytes = control_bytes(control_message::Payload::ConversationSyncRequest(
            ConversationSyncRequest {},
        ));
        self.send_control(bytes);
    }

    /// Adopt a steward's `ConversationSync` when we hold no steward list, or
    /// one that is exhausted at the current epoch. Reconstructs the candidate
    /// pool (members minus the carried unsettled set), re-derives the steward
    /// list to verify it, then applies list, protocol flags, timing and peer
    /// scores.
    pub(crate) fn on_conversation_sync(
        &mut self,
        sync: ConversationSync,
    ) -> Result<(), ConversationError> {
        // A sync is on the wire — the request round is answered, so disarm any
        // pending backup takeover before the guard returns.
        self.timing.sync_resend_anchor = None;
        let current_epoch = self.epoch;
        // Install a first list, or replace an exhausted one with a strictly
        // newer election (higher `election_epoch`).
        if let Some(mine) = self.steward_list.election_epoch()
            && (!self.steward_list.is_exhausted(current_epoch) || sync.election_epoch <= mine)
        {
            return Ok(());
        }
        if !validate_conversation_sync(&self.conversation_id, &sync, current_epoch, &self.members)?
        {
            return Ok(());
        }

        let stewards = sync.steward_members.len();
        self.adopt_conversation_sync(&sync)?;
        self.timing.unanswered_sync_rounds = 0;
        self.emit(Event::SyncApplied);
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

    /// Answer a sync re-send request. The epoch steward responds now; a backup
    /// arms `sync_resend_anchor` and takes over from `tick` only if the epoch
    /// steward stays silent. The list-present guard in
    /// [`Self::on_conversation_sync`] means only the degraded requester adopts
    /// the broadcast.
    pub(crate) fn on_conversation_sync_request(
        &mut self,
        requester: &[u8],
    ) -> Result<(), ConversationError> {
        if self.is_epoch_steward() {
            self.timing.sync_resend_anchor = None;
            tracing::debug!(
                conversation = %self.conversation_id,
                requester = ?requester,
                "epoch steward re-sending conversation sync"
            );
            return self.share_conversation_sync();
        }
        // A backup arms the takeover timer; a plain member can't answer.
        if self.is_steward() && self.timing.sync_resend_anchor.is_none() {
            self.timing.sync_resend_anchor = Some(self.now);
        }
        Ok(())
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
        // store: steward_list (WP3g)
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
        // Record the bootstrap scores, reporting how far each member had
        // already diverged from the default this node would have assumed.
        self.apply_score_snapshot(&snapshot);
        // store: scores (WP3g)
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
        // store: meta (WP3g)
        Ok(())
    }
}

/// Encode one `ControlMessage` payload for the router to seal.
pub(crate) fn control_bytes(payload: control_message::Payload) -> Vec<u8> {
    prost::Message::encode_to_vec(&ControlMessage {
        payload: Some(payload),
    })
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
    // Only accept unsettled members that actually exist, so fake ids can't
    // shrink the pool. Full protection needs every member to agree on the
    // sync's content.
    if !sync
        .unsettled_members
        .iter()
        .all(|u| members_set.contains(u.as_slice()))
    {
        info!(
            conversation = conversation_id,
            "conversation sync rejected: unsettled member is not in the current set"
        );
        return Ok(false);
    }
    let unsettled_set = member_set(&sync.unsettled_members);
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
