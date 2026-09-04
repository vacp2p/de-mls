//! The fake bed for de-mls integration tests: a cryptography-free MLS group
//! stand-in, the reference router built over it, and virtual-time network
//! delivery. Some methods here are scaffolding for scenarios not yet
//! written (design 9.2).
#![allow(dead_code)]

pub mod fake_mls;
pub mod fake_router;
pub mod net;
