//! The keys the engine writes to its [`crate::traits::EngineStore`] and
//! the per-call record of which ones changed. The engine owns every
//! encoding; the application supplies durability.

/// Keys the engine writes. Each holds one protobuf message.
pub mod keys {
    /// Steward list, election epoch, retry round.
    pub const STEWARD_LIST: &str = "steward_list";
    /// Approved proposals not yet committed, and the urgent commit target.
    pub const PROPOSALS: &str = "proposals";
    /// Member id to join epoch.
    pub const JOIN_EPOCHS: &str = "join_epochs";
    /// Stewards a deadlock vote skipped for the current epoch.
    pub const SKIPPED_STEWARDS: &str = "skipped_stewards";
    /// Peer scores.
    pub const SCORES: &str = "scores";
    /// Open consensus sessions: proposals and the votes seen so far.
    pub const CONSENSUS: &str = "consensus";
    /// Config and the epoch the snapshot was written at.
    pub const META: &str = "meta";
}

/// Which keys the current driving call changed; flushed by `finish`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Dirty {
    pub(crate) steward_list: bool,
    pub(crate) proposals: bool,
    pub(crate) join_epochs: bool,
    pub(crate) skipped_stewards: bool,
    pub(crate) scores: bool,
    pub(crate) consensus: bool,
    pub(crate) meta: bool,
}

impl Dirty {
    /// Every key, for a first write.
    pub(crate) const ALL: Dirty = Dirty {
        steward_list: true,
        proposals: true,
        join_epochs: true,
        skipped_stewards: true,
        scores: true,
        consensus: true,
        meta: true,
    };
}
