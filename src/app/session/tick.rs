//! [`SessionTick`] — wakeup-hint envelope returned by public ops on
//! [`crate::app::SessionRunner`] and [`crate::app::User`].

use std::time::Duration;

/// Earliest pending deadline relative to now, bundled into a named
/// return type. `None` when nothing is scheduled.
///
/// Public ops return this so callers can forward the value to an
/// external scheduler in one step. Events are not bundled — they stay
/// in the per-session pending buffer, drained via
/// [`crate::app::SessionRunner::drain_events`] independently. Keeping
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
/// - **Ops that open a consensus session** — [`User::initiate_proposal`],
///   [`User::add_member`], [`User::process_ban_request`],
///   [`User::process_user_vote`], [`User::initiate_self_leave`]: the tick
///   is when the local auto-vote casts or the consensus session times out.
///   Informally, "wait until we vote, then the steward commits."
/// - **Lifecycle / poll ops** — [`User::accept_welcome`],
///   [`User::handle_inbound`], [`User::poll_session`],
///   [`User::tick_deadlines`]: the tick is the earliest of *all* currently
///   armed deadlines (commit-inactivity, freeze, recovery-inactivity,
///   `PendingJoin` expiry) after this op re-derived them.
/// - **Pure sends** — [`User::push_message`], [`User::send_key_package`]:
///   nothing time-based is started, so the tick is usually `None`.
///
/// [`User::initiate_proposal`]: crate::app::User::initiate_proposal
/// [`User::add_member`]: crate::app::User::add_member
/// [`User::process_ban_request`]: crate::app::User::process_ban_request
/// [`User::process_user_vote`]: crate::app::User::process_user_vote
/// [`User::initiate_self_leave`]: crate::app::User::initiate_self_leave
/// [`User::accept_welcome`]: crate::app::User::accept_welcome
/// [`User::handle_inbound`]: crate::app::User::handle_inbound
/// [`User::poll_session`]: crate::app::User::poll_session
/// [`User::tick_deadlines`]: crate::app::User::tick_deadlines
/// [`User::push_message`]: crate::app::User::push_message
/// [`User::send_key_package`]: crate::app::User::send_key_package
#[derive(Debug, Default, Clone, Copy)]
pub struct SessionTick {
    pub next_wakeup_in: Option<Duration>,
}

impl SessionTick {
    pub fn empty() -> Self {
        Self::default()
    }
}
