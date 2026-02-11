//! Transport-agnostic topic filter used by the app as a fast allowlist.
use std::collections::HashSet;

use tokio::sync::RwLock;

use crate::ds::SUBTOPICS;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TopicKey {
    pub group_id: String,
    pub subtopic: String,
}

impl TopicKey {
    pub fn new(group_id: &str, subtopic: &str) -> Self {
        Self {
            group_id: group_id.to_string(),
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

    /// Add all subtopics for a group.
    pub async fn add_many(&self, group_name: &str) {
        let mut w = self.set.write().await;
        for sub in SUBTOPICS {
            w.insert(TopicKey::new(group_name, sub));
        }
    }

    /// Remove all subtopics for a group.
    pub async fn remove_many(&self, group_name: &str) {
        self.set
            .write()
            .await
            .retain(|x| x.group_id != group_name);
    }

    /// Membership test (first-stage filter).
    #[inline]
    pub async fn contains(&self, group_id: &str, subtopic: &str) -> bool {
        let key = TopicKey::new(group_id, subtopic);
        self.set.read().await.contains(&key)
    }

    pub async fn snapshot(&self) -> Vec<TopicKey> {
        self.set.read().await.iter().cloned().collect()
    }
}
