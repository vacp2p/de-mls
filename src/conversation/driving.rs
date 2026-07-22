//! The app-facing driving API — how an integrator advances a [`Conversation`]
//! over time.
//!
//! de-mls keeps no liveness timers. Each polling cycle:
//! 1. call [`Conversation::poll`] — it resolves votes, advances any in-flight
//!    commit round, and drives reelection internally;
//! 2. pull a trigger when your own timer or signal says to act.
//!
//! **Triggers self-gate — the queries are optional.** Every trigger is a no-op
//! when there's nothing to do ([`commit_now`](Conversation::commit_now) returns
//! `Ok(false)` outside `Working`, with no approved work, or with an election in
//! flight). So the simplest integrator just calls the trigger each cycle. The
//! paired query lets you *peek without acting* — to run a delay before the
//! trigger, or show a "N pending" count in a UI:
//!
//! ```ignore
//! // Immediate: commit as soon as there's work (the call no-ops otherwise).
//! convo.commit_now(&provider, &signer)?;
//!
//! // Delayed: peek, then batch proposals over your own commit-inactivity window.
//! if convo.pending_commit_work().is_some() && my_commit_timer.elapsed() {
//!     convo.commit_now(&provider, &signer)?;
//! }
//! ```
//!
//! Query → trigger pairs (the query is the optional peek):
//! - [`pending_commit_work`](Conversation::pending_commit_work) → [`commit_now`](Conversation::commit_now) — approved batch to commit
//! - [`pending_buffered_updates`](Conversation::pending_buffered_updates) → [`propose_buffered_updates`](Conversation::propose_buffered_updates) — buffered joins/removes (backup)
//! - [`pending_sync_resend`](Conversation::pending_sync_resend) → [`share_conversation_sync`](Conversation::share_conversation_sync) — unanswered sync request (backup)
//! - [`in_recovery_posture`](Conversation::in_recovery_posture) → [`commit_in_recovery`](Conversation::commit_in_recovery) — mint in open Layer-3 recovery
//! - [`ConversationEvent::ReelectionExhausted`](crate::ConversationEvent) → [`request_recovery`](Conversation::request_recovery) — open Layer-3 when reelection can't elect a steward
//! - *(hold a candidate)* → [`resend_commit`](Conversation::resend_commit) — re-broadcast it
//!
//! Reelection is **not** an app trigger: `poll()` advances the retry ladder
//! itself (the round re-seeds the shared steward list, so it must move in
//! lockstep). [`pending_reelection`](Conversation::pending_reelection) is
//! read-only status a UI can surface.

use std::error::Error as StdError;

use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationState, PeerScoreStorage,
    WallClock,
};

impl<Cp, Sc, Wc> Conversation<Cp, Sc, Wc>
where
    Cp: ConsensusPlugin,
    Sc: PeerScoreStorage,
    Wc: WallClock,
{
    /// Start the commit round now: enter `Freezing` and, if we're a steward,
    /// mint our candidate from the approved batch. Call it when your liveness
    /// policy decides the epoch steward has sat on approved work long enough, or
    /// when a backup takes over a silent primary.
    ///
    /// Self-gating, so it's safe to call every cycle: `Ok(false)` when there's
    /// nothing to do (not in `Working`, no approved work, or an election in
    /// flight — committing on a stale list only yields a NoCandidate), `Ok(true)`
    /// once the freeze is entered. Pair with
    /// [`pending_commit_work`](Self::pending_commit_work) only when you want to
    /// *wait* (a commit-inactivity delay) before firing.
    pub fn commit_now<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<bool, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if self.current_state() != ConversationState::Working
            || self.queues.approved_proposals_count() == 0
            || self.queues.has_election_in_flight()
        {
            return Ok(false);
        }
        let Some(event) = self.start_freezing() else {
            return Ok(false);
        };
        self.on_freeze_entered(provider, signer, event)?;
        Ok(true)
    }

    /// Manually produce a Layer-3 recovery commit: mint our candidate and queue
    /// its broadcast (only valid in recovery, where the steward gate is relaxed).
    /// `Ok(true)` if a candidate was produced, `Ok(false)` if there was nothing
    /// to commit or we already hold a local candidate this round — so the app
    /// can call it every cycle in recovery without minting duplicates.
    pub fn commit_in_recovery<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<bool, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if !self.is_in_recovery_mode() {
            return Err(ConversationError::NotInRecovery);
        }
        if self.queues.commit_round.has_local_candidate() {
            return Ok(false);
        }
        self.mint_and_broadcast_candidate(provider, signer)
    }

    /// Re-broadcast our commit candidate for the current round. `Ok(true)` if a
    /// candidate was re-sent, `Ok(false)` when we hold none for this round
    /// (nothing minted yet, or it already merged and cleared the buffer).
    pub fn resend_commit(&self) -> Result<bool, ConversationError> {
        match self.queues.commit_round.local_candidate_bytes() {
            Some(payload) => {
                self.broadcast(payload);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
