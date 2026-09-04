//! Steward-list housekeeping on the engine: list reconciliation and
//! elections, and the unstick and score-removal emergency proposals live in
//! [`list`]; the pending-update buffer and its drains live in [`updates`].
//! `ConversationSync` bootstrap and adoption live in [`sync`]; the
//! time-driven backup takeovers in [`drives`].

mod drives;
mod list;
mod sync;
mod updates;

#[cfg(test)]
pub(crate) use sync::control_bytes;

#[cfg(test)]
mod tests;
