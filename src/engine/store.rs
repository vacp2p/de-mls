//! The one storage trait the engine takes: a per-conversation key-value
//! store of bytes. The engine defines the keys ([`keys`]) and owns every
//! encoding; the integrator supplies durability.

use std::{collections::HashMap, convert::Infallible, error::Error};

/// Keys the engine writes. Each holds one protobuf message.
pub mod keys {
    /// Steward list, election epoch, retry round.
    pub const STEWARD_LIST: &str = "steward_list";
    /// Approved proposals not yet committed, buffered adds and removes.
    pub const PROPOSALS: &str = "proposals";
    /// Member id to join epoch.
    pub const JOIN_EPOCHS: &str = "join_epochs";
    /// Peer scores.
    pub const SCORES: &str = "scores";
    /// Open consensus sessions: proposals and the votes seen so far.
    pub const CONSENSUS: &str = "consensus";
    /// Config and the snapshot epoch.
    pub const META: &str = "meta";
}

/// Durable bytes under engine-defined keys, scoped to one conversation.
///
/// The engine writes only the key whose state changed, at the end of the
/// driving call that changed it, and reads every key back on construction.
/// The router makes a call's writes durable before sending that call's
/// outbound.
pub trait EngineStore {
    /// Backend I/O error. [`Infallible`] for a backend that cannot fail.
    type Error: Error + Send + Sync + 'static;

    /// The bytes under `key`, or `None` if never written.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Replace the bytes under `key`.
    fn put(&mut self, key: &str, value: &[u8]) -> Result<(), Self::Error>;
}

/// Process-lifetime store for tests and integrators without persistence.
#[derive(Debug, Clone, Default)]
pub struct InMemoryStore {
    entries: HashMap<String, Vec<u8>>,
}

impl EngineStore for InMemoryStore {
    type Error = Infallible;

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Infallible> {
        Ok(self.entries.get(key).cloned())
    }

    fn put(&mut self, key: &str, value: &[u8]) -> Result<(), Infallible> {
        self.entries.insert(key.to_string(), value.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_round_trip() {
        let mut store = InMemoryStore::default();
        assert_eq!(store.get(keys::META).unwrap(), None);
        store.put(keys::META, b"x").unwrap();
        assert_eq!(store.get(keys::META).unwrap().as_deref(), Some(&b"x"[..]));
        store.put(keys::META, b"y").unwrap();
        assert_eq!(store.get(keys::META).unwrap().as_deref(), Some(&b"y"[..]));
    }
}
