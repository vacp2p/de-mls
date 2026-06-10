//! Shared fixtures for the gateway integration suite.
//!
//! Rust compiles each file in `tests/` as its own binary; a file under
//! `tests/common/` is *not* a test binary, so this module is reused by
//! adding `mod common;` to any test file. Helpers carry `#[allow(dead_code)]`
//! at the module level because not every binary exercises every helper.
//!
//! [`session_fixtures`] drives the reference integrator (`User` +
//! `SessionRunner`) through `handle_inbound` / `receive_key_package`, with
//! transport capture and polling helpers. [`wallet`] supplies the test
//! `MemberId` adapter and a `User` constructor keyed by a private key.
#![allow(dead_code, unused_imports)]

pub mod session_fixtures;
pub mod wallet;

pub use wallet::WalletMemberId;
