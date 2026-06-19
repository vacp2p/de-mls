//! Ergonomic lock acquisition that propagates poisoning as `UserError`.
//!
//! `Conversation`'s per-conversation lock is `std::sync::RwLock`, which
//! marks itself poisoned if a holder panics. We never want to swallow that
//! with `.expect(...)`; the helpers below convert poison errors into
//! [`UserError::LockPoisoned`] so callers can return them upstream.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::user::UserError;

pub(crate) trait LockExt<T> {
    /// Acquire a read guard. Returns `UserError::LockPoisoned(ctx)` if poisoned.
    fn read_or_err(&self, ctx: &'static str) -> Result<RwLockReadGuard<'_, T>, UserError>;
    /// Acquire a write guard. Returns `UserError::LockPoisoned(ctx)` if poisoned.
    fn write_or_err(&self, ctx: &'static str) -> Result<RwLockWriteGuard<'_, T>, UserError>;
}

impl<T> LockExt<T> for RwLock<T> {
    fn read_or_err(&self, ctx: &'static str) -> Result<RwLockReadGuard<'_, T>, UserError> {
        self.read().map_err(|_| UserError::LockPoisoned(ctx))
    }

    fn write_or_err(&self, ctx: &'static str) -> Result<RwLockWriteGuard<'_, T>, UserError> {
        self.write().map_err(|_| UserError::LockPoisoned(ctx))
    }
}
