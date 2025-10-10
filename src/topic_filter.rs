use ds::build_content_topics;
// de-mls/src/topic_filter.rs
use tokio::sync::RwLock;
use waku_bindings::WakuContentTopic;

/// Fast allowlist for content topics without requiring Hash.
/// Internally uses a Vec and dedupes on insert.
#[derive(Default)]
pub struct TopicFilter {
    list: RwLock<Vec<WakuContentTopic>>,
}

impl TopicFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build and add topics if not already present.
    pub async fn add_many(&self, group_name: &str) {
        let topics = build_content_topics(group_name);
        self.list.write().await.extend(topics);
    }

    /// Remove any matching topics.
    pub async fn remove_many(&self, group_name: &str) {
        let topics = build_content_topics(group_name);
        self.list
            .write()
            .await
            .retain(|x| !topics.iter().any(|t| t == x));
    }

    /// Membership test (first-stage filter).
    #[inline]
    pub async fn contains(&self, t: &WakuContentTopic) -> bool {
        self.list.read().await.iter().any(|x| x == t)
    }

    pub async fn snapshot(&self) -> Vec<WakuContentTopic> {
        self.list.read().await.clone()
    }
}
