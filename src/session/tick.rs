//! [`SessionTick`] — wakeup-hint envelope returned by public ops on
//! [`crate::session::SessionRunner`].

use std::time::Duration;

/// Earliest pending deadline relative to now, bundled into a named
/// return type. `None` when nothing is scheduled.
///
/// Public ops return this so callers can forward the value to an
/// external scheduler in one step. Events are not bundled — they stay
/// in the per-session pending buffer, drained via
/// [`crate::session::SessionRunner::drain_events`] independently. Keeping
/// the two concerns separate lets internal session methods emit events
/// without coordinating with op return values.
///
/// # What the deadline means
///
/// `next_wakeup_in` is never a measure of the call's own success — it is
/// "the soonest moment something in this session will fire on its own (a
/// timer), so wake me by then." The returning op determines *which*
/// deadline that is:
///
/// - **Ops that open a consensus session** — `initiate_proposal`,
///   `add_member`, `process_ban_request`, `process_user_vote`: the tick
///   is when the local auto-vote casts or the consensus session times out.
///   Informally, "wait until we vote, then the steward commits."
/// - **`poll()`** — the tick is the earliest of *all* currently armed
///   deadlines (commit-inactivity, freeze, recovery-inactivity,
///   `PendingJoin` expiry) after all sub-steps have run.
/// - **Pure sends** — `push_message`, `send_key_package`:
///   nothing time-based is started, so the tick is usually `None`.
///
#[derive(Debug, Default, Clone, Copy)]
pub struct SessionTick {
    pub next_wakeup_in: Option<Duration>,
}

impl SessionTick {
    pub fn empty() -> Self {
        Self::default()
    }
}
