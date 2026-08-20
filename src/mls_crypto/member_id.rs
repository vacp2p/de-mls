//! Canonical `member_id` encoding: a member's identity is its ratchet-tree
//! position.
//!
//! Throughout de-mls a `member_id` is the big-endian `u32` encoding of the
//! member's OpenMLS `LeafNodeIndex`. The index is stable across credential and
//! key rotation (an in-place leaf update keeps the position), identical on every
//! node (the ratchet tree is the same at an epoch), and authenticated (it is
//! read from `sender()`), so it is the one durable, agreed name for a member.
//! The id-agnostic steward-list, scoring, consensus, and wire layers carry these
//! bytes opaquely; only this module and [`MlsService`](super::MlsService) know
//! they encode a leaf index.

use openmls::prelude::{LeafNodeIndex, Member};

/// An opaque, de-mls-issued handle to a current member. Obtain one with
/// [`MemberId::from`] a [`Member`] the library returned; pass it back to
/// `remove_member` / `member_score`. It pins the member's leaf and the
/// signature key held when issued, so once that member leaves or its leaf is
/// reused the handle stops resolving instead of acting on a different member.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct MemberId {
    leaf: LeafNodeIndex,
    tag: Vec<u8>,
}

impl MemberId {
    pub(crate) fn leaf(&self) -> LeafNodeIndex {
        self.leaf
    }

    pub(crate) fn tag(&self) -> &[u8] {
        &self.tag
    }
}

impl From<&Member> for MemberId {
    fn from(member: &Member) -> Self {
        Self {
            leaf: member.index,
            tag: member.signature_key.clone(),
        }
    }
}

/// Encode a leaf index as its `member_id` bytes.
pub(crate) fn member_id_of(index: LeafNodeIndex) -> Vec<u8> {
    index.u32().to_be_bytes().to_vec()
}

/// Decode `member_id` bytes back to a leaf index, or `None` when they are not a
/// 4-byte encoding.
pub(crate) fn leaf_index_of(member_id: &[u8]) -> Option<LeafNodeIndex> {
    let bytes: [u8; 4] = member_id.try_into().ok()?;
    Some(LeafNodeIndex::new(u32::from_be_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_bytes() {
        for raw in [0u32, 1, 7, 255, 256, u32::MAX] {
            let idx = LeafNodeIndex::new(raw);
            let bytes = member_id_of(idx);
            assert_eq!(bytes.len(), 4);
            assert_eq!(leaf_index_of(&bytes), Some(idx));
        }
    }

    #[test]
    fn rejects_wrong_width() {
        assert_eq!(leaf_index_of(&[]), None);
        assert_eq!(leaf_index_of(&[1, 2, 3]), None);
        assert_eq!(leaf_index_of(&[1, 2, 3, 4, 5]), None);
    }
}
