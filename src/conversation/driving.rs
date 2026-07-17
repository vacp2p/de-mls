//! The app-facing driving API — how an integrator advances a [`Conversation`]
//! over time.
//!
//! de-mls keeps no liveness timers. Each polling cycle the app:
//! 1. calls [`Conversation::poll`] (agreement deadlines + freeze progression),
//! 2. reads the condition queries to see what liveness action is available, and
//! 3. pulls the matching trigger once its own timer/signal says to act.
//!
//! Condition queries (in `query.rs`):
//! - [`Conversation::pending_commit_work`] — approved batch waiting to commit
//! - [`Conversation::in_recovery_posture`] — commit on the short recovery window
//! - [`Conversation::pending_reelection`] — a silent reelection round
//! - [`Conversation::pending_sync_resend`] — an unanswered sync request (backup)
//! - [`Conversation::pending_buffered_updates`] — buffered joins/removes to propose
//!
//! Triggers — the levers the app pulls:
//! - [`Conversation::commit_now`] — start the commit round now (this module)
//! - [`Conversation::resend_commit`] — re-broadcast a held candidate (this module)
//! - [`Conversation::commit_in_recovery`] — mint in open Layer-3 recovery (this module)
//! - [`Conversation::advance_election_retry`] — count a silent reelection round rejected (`steward.rs`)
//! - [`Conversation::share_conversation_sync`] — re-send the sync as a backup (`steward.rs`)
//! - [`Conversation::propose_buffered_updates`] — drain the buffer as a backup (`steward.rs`)
//! - [`Conversation::request_recovery`] — open Layer-3 recovery (`steward.rs`)

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
    /// Start the commit round now instead of waiting out a commit-inactivity
    /// timer: enter `Freezing` and, if we're a steward, mint our candidate from
    /// the approved batch. de-mls no longer keeps this timer — the app calls
    /// this when its own liveness policy decides the epoch steward has sat on
    /// approved work long enough, or when a backup takes over a silent primary.
    ///
    /// `Ok(true)` when the freeze was entered, `Ok(false)` when there is nothing
    /// to do: not in `Working`, no approved work, or an election in flight
    /// (committing on a stale steward list only produces a NoCandidate). Mirrors
    /// the guards of [`Self::pending_commit_work`].
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
    /// can call it every cycle in recovery without minting duplicates. de-mls no
    /// longer auto-mints; the app owns the recovery-commit timing.
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

    /// Re-broadcast our commit candidate for the current round, so members that
    /// dropped the original receive it and apply it — the steward's alternative
    /// to survivors minting a competing commit, which forks. `Ok(true)` if a
    /// candidate was re-sent, `Ok(false)` when we hold none for this round
    /// (nothing minted yet, or it already merged and cleared the buffer). de-mls
    /// does not time this; the app calls it when its liveness policy judges the
    /// commit was likely dropped.
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
