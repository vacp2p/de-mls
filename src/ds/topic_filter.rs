//! Transport-agnostic topic filter used by the app as a fast allowlist.
use std::collections::HashSet;

use tokio::sync::RwLock;

use crate::ds::SUBTOPICS;

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
    pub async fn add_many(&self, conversation_name: &str) {
        let mut w = self.set.write().await;
        for sub in SUBTOPICS {
            w.insert(TopicKey::new(conversation_name, sub));
        }
    }

    /// Remove all subtopics for a conversation.
    pub async fn remove_many(&self, conversation_name: &str) {
        self.set
            .write()
            .await
            .retain(|x| x.conversation_id != conversation_name);
    }

    /// Membership test (first-stage filter).
    #[inline]
    pub async fn contains(&self, conversation_id: &str, subtopic: &str) -> bool {
        let key = TopicKey::new(conversation_id, subtopic);
        self.set.read().await.contains(&key)
    }

    pub async fn snapshot(&self) -> Vec<TopicKey> {
        self.set.read().await.iter().cloned().collect()
    }
}
