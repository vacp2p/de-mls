//! The commit round over facts: the cheap admission gate the router asks
//! before staging, the local mint report, and the merge report that moves
//! the engine's own view of the group, in [`calls`]. The buffer lives in
//! [`buffer`], round selection in [`select`], and batching our own
//! candidate in [`batch`].

mod batch;
mod buffer;
mod calls;
mod select;

pub(crate) use buffer::CommitRoundBuffer;

#[cfg(test)]
mod tests;
