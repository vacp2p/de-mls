//! Everything that talks to the consensus library: opening proposals and
//! casting votes ([`proposals`]), the wire shapes and session opening
//! ([`wire`]), the auto-vote and timeout deadlines ([`deadlines`]), what a
//! resolved proposal does to the queues ([`apply`]) and the follow-up the
//! engine then runs ([`dispatch`]), the outcome channel between the library
//! and the engine ([`outcome_bus`]), and the identity-only vote signer
//! ([`signer`]).

mod apply;
mod deadlines;
mod dispatch;
mod outcome_bus;
mod proposals;
mod signer;
mod wire;

pub(crate) use outcome_bus::{OutcomeBus, OutcomeReceiver};
pub(crate) use signer::MemberSigner;

#[cfg(test)]
mod tests;
