//! Ergonomic lock acquisition that propagates poisoning as `SessionError`.
//!
//! `SessionRunner`'s per-conversation lock is `std::sync::RwLock`, which
//! marks itself poisoned if a holder panics. We never want to swallow that
//! with `.expect(...)`; the helpers below convert poison errors into
//! [`SessionError::LockPoisoned`] so callers can return them upstream.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use de_mls::session::SessionError;

pub(crate) trait LockExt<T> {
    /// Acquire a read guard. Returns `SessionError::LockPoisoned(ctx)` if poisoned.
    fn read_or_err(&self, ctx: &'static str) -> Result<RwLockReadGuard<'_, T>, SessionError>;
    /// Acquire a write guard. Returns `SessionError::LockPoisoned(ctx)` if poisoned.
    fn write_or_err(&self, ctx: &'static str) -> Result<RwLockWriteGuard<'_, T>, SessionError>;
}

impl<T> LockExt<T> for RwLock<T> {
    fn read_or_err(&self, ctx: &'static str) -> Result<RwLockReadGuard<'_, T>, SessionError> {
        self.read().map_err(|_| SessionError::LockPoisoned(ctx))
    }

    fn write_or_err(&self, ctx: &'static str) -> Result<RwLockWriteGuard<'_, T>, SessionError> {
        self.write().map_err(|_| SessionError::LockPoisoned(ctx))
    }
}
