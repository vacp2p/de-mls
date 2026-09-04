//! The wire between nodes: typed frames, broadcast delivery with a fixed
//! delay, and partitions by muting a node.

use std::{collections::VecDeque, time::Duration};

use de_mls::engine::{CommitHash, MemberId, Timestamp};

use crate::common::fake_mls::{Sealed, Snapshot};

/// A welcome as the application delivers it: the group snapshot its joiners
/// adopt. It carries nothing else — a joiner starts unsynced and adopts the
/// first `ConversationSync` a steward broadcasts after the commit that seated
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub joiners: Vec<MemberId>,
    pub for_commit: CommitHash,
    pub snapshot: Snapshot,
}

/// One payload on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Sealed chat or control.
    Sealed(Sealed),
    /// Raw MLS commit bytes, broadcast in the clear.
    Commit(Vec<u8>),
    /// A welcome, delivered to its joiners only.
    Welcome(Welcome),
}

#[derive(Debug, Clone)]
struct Packet {
    from: usize,
    frame: Frame,
    due: Timestamp,
}

/// Broadcast network with per-node partitions.
#[derive(Debug, Default)]
pub struct Net {
    queue: VecDeque<Packet>,
    muted: Vec<bool>,
    delay: Duration,
}

impl Net {
    pub fn new(nodes: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            muted: vec![false; nodes],
            delay: Duration::ZERO,
        }
    }

    /// Register one more node.
    pub fn add_node(&mut self) {
        self.muted.push(false);
    }

    /// Every later broadcast arrives `delay` after it was sent.
    pub fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    /// A muted node neither sends nor receives.
    pub fn mute(&mut self, node: usize) {
        self.muted[node] = true;
    }

    pub fn unmute(&mut self, node: usize) {
        self.muted[node] = false;
    }

    pub fn is_muted(&self, node: usize) -> bool {
        self.muted[node]
    }

    /// Queue `frame` from `from` for everyone else.
    pub fn broadcast(&mut self, now: Timestamp, from: usize, frame: Frame) {
        if self.muted[from] {
            return;
        }
        self.queue.push_back(Packet {
            from,
            frame,
            due: now + self.delay,
        });
    }

    /// Packets due by `now`, in send order, as `(from, frame)`.
    pub fn take_due(&mut self, now: Timestamp) -> Vec<(usize, Frame)> {
        let mut due = Vec::new();
        let mut later = VecDeque::new();
        while let Some(packet) = self.queue.pop_front() {
            if packet.due <= now {
                due.push((packet.from, packet.frame));
            } else {
                later.push_back(packet);
            }
        }
        self.queue = later;
        due
    }

    pub fn in_flight(&self) -> usize {
        self.queue.len()
    }
}
