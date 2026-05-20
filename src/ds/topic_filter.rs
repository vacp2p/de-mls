//! Transport-agnostic topic filter used by the app as a fast allowlist.
use std::collections::HashSet;
use std::sync::RwLock;

use crate::ds::{DeliveryServiceError, SUBTOPICS};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TopicKey {
    pub conversation_id: String,
    pub subtopic: String,
}

impl TopicKey {
    pub fn new(conversation_id: &str, subtopic: &str) -> Self {
        Self {
            conversation_id: conversation_id.to_string(),
            subtopic: subtopic.to_string(),
        }
    }
}

/// Fast allowlist for inbound routing.
#[derive(Default, Debug)]
pub struct TopicFilter {
    set: RwLock<HashSet<TopicKey>>,
}

impl TopicFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add all subtopics for a conversation.
    pub fn add_many(&self, conversation_name: &str) -> Result<(), DeliveryServiceError> {
        let mut w = self
            .set
            .write()
            .map_err(|_| DeliveryServiceError::LockPoisoned("TopicFilter"))?;
        for sub in SUBTOPICS {
            w.insert(TopicKey::new(conversation_name, sub));
        }
        Ok(())
    }

    /// Remove all subtopics for a conversation.
    pub fn remove_many(&self, conversation_name: &str) -> Result<(), DeliveryServiceError> {
        self.set
            .write()
            .map_err(|_| DeliveryServiceError::LockPoisoned("TopicFilter"))?
            .retain(|x| x.conversation_id != conversation_name);
        Ok(())
    }

    /// Membership test (first-stage filter).
    #[inline]
    pub fn contains(
        &self,
        conversation_id: &str,
        subtopic: &str,
    ) -> Result<bool, DeliveryServiceError> {
        let key = TopicKey::new(conversation_id, subtopic);
        Ok(self
            .set
            .read()
            .map_err(|_| DeliveryServiceError::LockPoisoned("TopicFilter"))?
            .contains(&key))
    }

    pub fn snapshot(&self) -> Result<Vec<TopicKey>, DeliveryServiceError> {
        Ok(self
            .set
            .read()
            .map_err(|_| DeliveryServiceError::LockPoisoned("TopicFilter"))?
            .iter()
            .cloned()
            .collect())
    }
}
