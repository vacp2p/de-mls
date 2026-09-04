//! The commit round over facts: the driving calls the router makes —
//! `handle_candidate`, `commit_applied`, `decision_failed` — live in
//! [`calls`]; round closing in [`select`]; batching our own candidate in
//! [`batch`].

mod batch;
mod calls;
mod select;

#[cfg(test)]
mod tests;
