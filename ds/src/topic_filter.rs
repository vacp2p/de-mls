//! Transport-agnostic topic filter used by the app as a fast allowlist.
use tokio::sync::RwLock;

use crate::SUBTOPICS;

#[derive(Debug, Clone, PartialEq, Eq)]
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
/// Internally uses a Vec and dedupes on insert.
#[derive(Default, Debug)]
pub struct TopicFilter {
    list: RwLock<Vec<TopicKey>>,
}

impl TopicFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add all subtopics for a group.
    pub async fn add_many(&self, group_name: &str) {
        let mut w = self.list.write().await;
        for sub in SUBTOPICS {
            let k = TopicKey::new(group_name, sub);
            if !w.iter().any(|x| x == &k) {
                w.push(k);
            }
        }
    }

    /// Remove all subtopics for a group.
    pub async fn remove_many(&self, group_name: &str) {
        self.list
            .write()
            .await
            .retain(|x| x.group_id != group_name);
    }

    /// Membership test (first-stage filter).
    #[inline]
    pub async fn contains(&self, group_id: &str, subtopic: &str) -> bool {
        self.list
            .read()
            .await
            .iter()
            .any(|x| x.group_id == group_id && x.subtopic == subtopic)
    }

    pub async fn snapshot(&self) -> Vec<TopicKey> {
        self.list.read().await.clone()
    }
}
