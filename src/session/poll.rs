//! Unified per-cycle polling entry point for [`crate::session::SessionRunner`].
//!
//! [`SessionRunner::poll`] drives all time-based session paths in one call:
//! consensus-deadline ticks, freeze-status progression, member-freeze
//! detection, and pending-join expiry. The integrator calls it once per
//! wakeup cycle and reacts to the returned [`PollOutcome`].

use std::time::Duration;

use crate::{
    core::{ConsensusPlugin, ConversationPluginsFactory, SessionEvent},
    session::{
        DispatchOutcome, FreezeTimeoutStatus, SessionError, SessionRunner, freeze::PendingJoinTick,
    },
};

/// Summary returned by [`SessionRunner::poll`] after one polling pass.
#[derive(Debug, Clone)]
pub struct PollOutcome {
    /// Earliest deadline still pending after this pass. Forward to an
    /// external scheduler as the next wakeup hint.
    pub next_wakeup_in: Option<Duration>,
    /// `true` if this session should be torn down: either a `PendingJoin`
    /// expiry or a commit that ejected the local member. The integrator
    /// must call `User::finalize_self_leave` before its next polling cycle.
    pub leave_requested: bool,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Drive one polling cycle: tick consensus deadlines, advance freeze
    /// state, check member-freeze inactivity, and check pending-join expiry.
    ///
    /// Best-effort for non-fatal errors — each step runs regardless of
    /// whether the previous one failed. Fatal errors (`ConversationNotFound`,
    /// `LockPoisoned`) stop the pass and propagate immediately.
    ///
    /// Returns [`PollOutcome::leave_requested`] when the session is ready
    /// to be torn down; the integrator runs `User::finalize_self_leave`.
    pub fn poll(&mut self) -> Result<PollOutcome, SessionError> {
        let mut leave_requested = false;

        match self.tick_deadlines() {
            Ok(_) => {}
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => tracing::warn!(
                conversation = %self.conversation_id,
                error = %e,
                "tick_deadlines error in poll"
            ),
        }

        let freeze_status_opt = match self.poll_freeze_status() {
            Ok((status, dispatch)) => {
                if matches!(dispatch, DispatchOutcome::LeaveRequested) {
                    leave_requested = true;
                }
                Some(status)
            }
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => {
                tracing::warn!(
                    conversation = %self.conversation_id,
                    error = %e,
                    "poll_freeze_status error in poll"
                );
                None
            }
        };

        // Emit FreezeProgress only when count changed, reset on exit.
        if let Some(FreezeTimeoutStatus::StillFreezing) = freeze_status_opt {
            let (received, expected) = self.get_freeze_candidate_count();
            let progress = (received, expected);
            if self.last_freeze_progress != Some(progress) {
                self.last_freeze_progress = Some(progress);
                self.emit_event(SessionEvent::FreezeProgress { received, expected });
            }
        } else if freeze_status_opt.is_some() {
            self.last_freeze_progress = None;
        }

        if matches!(freeze_status_opt, Some(FreezeTimeoutStatus::NotFreezing)) {
            match self.check_member_freeze() {
                Ok(_) => {}
                Err(e) if e.is_fatal() => return Err(e),
                Err(e) => tracing::warn!(
                    conversation = %self.conversation_id,
                    error = %e,
                    "check_member_freeze error in poll"
                ),
            }
        }

        match self.check_pending_join() {
            Ok(PendingJoinTick::Expired) => leave_requested = true,
            Ok(_) => {}
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => tracing::warn!(
                conversation = %self.conversation_id,
                error = %e,
                "check_pending_join error in poll"
            ),
        }

        Ok(PollOutcome {
            next_wakeup_in: self.next_wakeup_in(),
            leave_requested,
        })
    }
}
