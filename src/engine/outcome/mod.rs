//! What a resolved consensus proposal does: the queue effects it has, and
//! the follow-up the engine still owes once those are applied.
//!
//! [`dispatch`] is the acting half: it reports the decision, runs
//! [`apply::apply_outcome`], and then runs the follow-up. [`apply`] is the
//! pure half: given the decision and the request it carried, it reshapes
//! `EngineQueues` and names the follow-up `dispatch` still owes.

mod apply;
mod dispatch;

#[cfg(test)]
mod tests;
