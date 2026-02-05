use std::collections::HashSet;
use tokio::sync::RwLock;

#[derive(Default, Debug)]
pub struct GroupRegistry {
    names: RwLock<HashSet<String>>,
}

impl GroupRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn exists(&self, name: &str) -> bool {
        self.names.read().await.contains(name)
    }

    pub async fn insert(&self, name: String) -> bool {
        let mut g = self.names.write().await;
        if g.contains(&name) {
            return false;
        }
        g.insert(name);
        true
    }

    pub async fn remove(&self, name: &str) {
        self.names.write().await.remove(name);
    }

    pub async fn all(&self) -> Vec<String> {
        self.names.read().await.iter().cloned().collect()
    }
}
