//! Cross-cutting helpers shared by the engine's queues and proposal paths:
//! the deterministic single-voter proposal id and a borrowed member-set view.

use std::collections::HashSet;

use sha2::{Digest, Sha256};

/// Deterministic id for a single-voter proposal, from `seed`: the leaver's
/// id for a self-leave, the update's bytes for a leaf update. Every node
/// keys the approved entry alike and a retransmit collides.
pub(crate) fn single_voter_proposal_id(seed: &[u8]) -> u32 {
    let hash = Sha256::digest(seed);
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
}

/// Borrow-only `HashSet` view over a slice of member_id blobs, for O(1)
/// membership lookups against `Vec<Vec<u8>>`.
pub(crate) fn member_set(members: &[Vec<u8>]) -> HashSet<&[u8]> {
    members.iter().map(|m| m.as_slice()).collect()
}
