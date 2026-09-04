//! The pending-update buffer every member records so a future epoch steward
//! can retry a stranded membership change, and the urgent-commit target an
//! ECP narrows the next freeze cycle to.

use std::collections::HashMap;

use crate::engine::{
    queues::{EngineQueues, PendingUpdate},
    util::target_member_id_of,
};
use crate::protos::de_mls::messages::v1::ConversationUpdateRequest;

impl EngineQueues {
    // ─────────────────────────── Pending Update Buffer ───────────────────────────

    /// Insert a `ConversationUpdateRequest` into the pending-updates buffer.
    ///
    /// Keyed by target user — a second insertion for the same user
    /// keeps the original `first_seen_epoch` so it can still expire on schedule.
    /// Returns `true` if this is a new entry, `false` if already buffered.
    pub fn insert_pending_update(
        &mut self,
        request: ConversationUpdateRequest,
        current_epoch: u64,
    ) -> bool {
        let Some(key) = target_member_id_of(&request) else {
            return false;
        };
        if self.pending_updates.contains_key(&key) {
            return false;
        }
        self.pending_updates.insert(
            key,
            PendingUpdate {
                request,
                first_seen_epoch: current_epoch,
            },
        );
        true
    }

    /// Drop a buffered update by target member_id.
    ///
    /// Returns `true` if an entry was removed.
    pub fn remove_pending_update(&mut self, member_id: &[u8]) -> bool {
        self.pending_updates.remove(member_id).is_some()
    }

    /// Check whether a pending update exists for the given member_id.
    #[cfg(test)]
    pub fn has_pending_update(&self, member_id: &[u8]) -> bool {
        self.pending_updates.contains_key(member_id)
    }

    /// Read-only access to the pending-updates buffer.
    pub fn pending_updates(&self) -> &HashMap<Vec<u8>, PendingUpdate> {
        &self.pending_updates
    }

    /// Number of buffered pending updates.
    pub fn pending_update_count(&self) -> usize {
        self.pending_updates.len()
    }

    /// Drop entries whose `first_seen_epoch` is older than `current_epoch - max_age`.
    ///
    /// Returns the member ids of expired entries.
    pub fn expire_pending_updates(&mut self, current_epoch: u64, max_age: u32) -> Vec<Vec<u8>> {
        let cutoff = current_epoch.saturating_sub(max_age as u64);
        let expired: Vec<Vec<u8>> = self
            .pending_updates
            .iter()
            .filter(|(_, p)| p.first_seen_epoch < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &expired {
            self.pending_updates.remove(k);
        }
        expired
    }

    // ─────────────────────────── Urgent (ECP-driven) Commit Target ───────────────────────────

    pub fn urgent_commit_target(&self) -> Option<&[u8]> {
        self.urgent_commit_target.as_deref()
    }

    /// Mark the next freeze cycle as urgent and committed-only-for `target`.
    pub(crate) fn set_urgent_commit_target(&mut self, target: Vec<u8>) {
        self.urgent_commit_target = Some(target);
    }

    pub(crate) fn take_urgent_commit_target(&mut self) -> Option<Vec<u8>> {
        self.urgent_commit_target.take()
    }
}
