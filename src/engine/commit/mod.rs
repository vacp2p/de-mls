//! The commit round over facts: the driving calls the router makes —
//! `handle_candidate`, `commit_applied`, `decision_failed` — live in
//! [`calls`]; round closing in [`select`]; batching our own candidate in
//! [`batch`]; approved work the group refused at the build in
//! [`withdraw`].

mod batch;
mod calls;
mod select;
mod withdraw;

#[cfg(test)]
mod tests;
