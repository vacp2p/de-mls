//! Opening proposals, casting votes, and the adapters that talk to the
//! consensus service, in [`proposals`]. Wire-level construction
//! (control-message encoding, session opening) lives in [`wire`]; the
//! time-based follow-ups (auto-votes, consensus timeouts) that
//! `tick_deadlines` fires on a later tick live in [`deadlines`].

mod deadlines;
mod proposals;
mod wire;

pub(crate) use proposals::CreatorVote;

#[cfg(test)]
mod tests;
