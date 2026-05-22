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
#[derive(Debug, Default, Clone, Copy)]
pub struct SessionTick {
    pub next_wakeup_in: Option<Duration>,
}

impl SessionTick {
    pub fn empty() -> Self {
        Self::default()
    }
}
