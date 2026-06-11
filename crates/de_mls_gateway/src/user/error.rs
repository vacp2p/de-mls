//! Error type for the `User` registry layer.
//!
//! Conversation-level failures come from the library as
//! [`ConversationError`] and are wrapped; everything else here is
//! registry-side: conversation lookup, lock poisoning, and transport
//! delivery.

use de_mls::session::ConversationError;

/// Errors from `User` operations.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("Conversation already exists")]
    ConversationAlreadyExists,

    #[error("Conversation not found")]
    ConversationNotFound,

    #[error("Welcome does not address this user's key package")]
    WelcomeNotForUs,

    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Lock poisoned: {0}")]
    LockPoisoned(&'static str),

    #[error(transparent)]
    Conversation(#[from] ConversationError),
}

impl UserError {
    /// Returns `true` for errors that indicate the conversation is gone or
    /// the registry state is irrecoverable. A polling loop should stop on
    /// fatal errors; non-fatal errors are transient and the loop continues.
    pub fn is_fatal(&self) -> bool {
        match self {
            // The conversation is no longer in the registry — stop polling.
            UserError::ConversationNotFound => true,
            // Lock poisoning means the conversation is corrupted — no recovery.
            UserError::LockPoisoned(_) => true,
            UserError::ConversationAlreadyExists
            | UserError::WelcomeNotForUs
            | UserError::Transport(_)
            | UserError::Conversation(_) => false,
        }
    }
}
