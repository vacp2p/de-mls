//! The three traits the application implements for the engine:
//! [`GroupView`] over its MLS group, [`EngineStore`] over its storage,
//! [`Publish`] over its message delivery.

use std::{collections::HashMap, convert::Infallible, error::Error};

use crate::engine::MemberId;

/// Read access to the MLS group: who it is and who is in it right now.
///
/// Implement it on the MLS group itself. Every method answers for the
/// group's current epoch.
pub trait GroupView {
    /// The MLS group id.
    fn conversation_id(&self) -> &str;

    /// This member's signature public key.
    fn own_id(&self) -> MemberId;

    /// The current epoch of the group.
    fn epoch(&self) -> u64;

    /// The signature public key of every current member, this member
    /// included, in any order.
    fn members(&self) -> Vec<MemberId>;
}

/// Persistent key-value storage for one conversation.
///
/// Keys are short strings chosen by the engine, values are opaque bytes.
/// Give each conversation its own namespace: the engine uses the same
/// keys in every conversation. A `put` must be on disk when it returns,
/// since the engine sends messages right after writing what they depend
/// on.
pub trait EngineStore {
    /// Backend I/O error. [`Infallible`] for a backend that cannot fail.
    type Error: Error + Send + Sync + 'static;

    /// The bytes under `key`, or `None` if never written.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Replace the bytes under `key`.
    fn put(&mut self, key: &str, value: &[u8]) -> Result<(), Self::Error>;
}

/// Sending a message to every member of the group.
///
/// The engine hands over plaintext bytes. Encrypt them as an MLS
/// application message of the group and deliver them the same way
/// messages go.
///
/// Return an error only when the message cannot be handed to delivery at
/// all, for example when the group is closed. The engine reports it to
/// the application and continues; it does not retry. Delivery failures
/// that happen later belong to the application's own retry logic.
pub trait Publish {
    /// Why a message could not be handed to delivery.
    type Error: Error + Send + Sync + 'static;

    /// Encrypt `bytes` for the group and send them.
    fn publish(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;
}

/// A group view over fixed values, for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InMemoryGroupView {
    /// See [`GroupView::conversation_id`].
    pub conversation_id: String,
    /// See [`GroupView::own_id`].
    pub own_id: MemberId,
    /// See [`GroupView::epoch`].
    pub epoch: u64,
    /// See [`GroupView::members`].
    pub members: Vec<MemberId>,
}

impl GroupView for InMemoryGroupView {
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn own_id(&self) -> MemberId {
        self.own_id.clone()
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn members(&self) -> Vec<MemberId> {
        self.members.clone()
    }
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

/// A publisher that keeps every message instead of sending it, for tests.
#[derive(Debug, Clone, Default)]
pub struct InMemoryPublisher {
    /// Every message published so far, oldest first.
    pub messages: Vec<Vec<u8>>,
}

impl InMemoryPublisher {
    /// Every message published so far, leaving the publisher empty.
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.messages)
    }
}

impl Publish for InMemoryPublisher {
    type Error = Infallible;

    fn publish(&mut self, bytes: &[u8]) -> Result<(), Infallible> {
        self.messages.push(bytes.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::keys;

    #[test]
    fn in_memory_store_round_trip() {
        let mut store = InMemoryStore::default();
        assert_eq!(store.get(keys::META).unwrap(), None);
        store.put(keys::META, b"x").unwrap();
        assert_eq!(store.get(keys::META).unwrap().as_deref(), Some(&b"x"[..]));
        store.put(keys::META, b"y").unwrap();
        assert_eq!(store.get(keys::META).unwrap().as_deref(), Some(&b"y"[..]));
    }

    #[test]
    fn in_memory_group_view_reads_its_fields() {
        let own = MemberId::from(&b"a"[..]);
        let other = MemberId::from(&b"b"[..]);
        let view = InMemoryGroupView {
            conversation_id: "g".into(),
            own_id: own.clone(),
            epoch: 3,
            members: vec![own.clone(), other.clone()],
        };
        assert_eq!(view.conversation_id(), "g");
        assert_eq!(view.own_id(), own);
        assert_eq!(view.epoch(), 3);
        assert_eq!(view.members(), vec![own, other]);
    }

    #[test]
    fn in_memory_publisher_keeps_order_and_drains() {
        let mut publisher = InMemoryPublisher::default();
        publisher.publish(b"1").unwrap();
        publisher.publish(b"2").unwrap();
        assert_eq!(publisher.drain(), vec![b"1".to_vec(), b"2".to_vec()]);
        assert!(publisher.messages.is_empty());
    }
}
