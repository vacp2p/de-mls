//! The per-epoch commit-round buffer: candidates collected so far and the
//! envelopes admitted for staging whose facts have not come back yet.

use crate::{
    engine::types::{CommitHash, StagedFacts},
    protos::de_mls::messages::v1::CommitCandidate,
};

/// One candidate in the round: the envelope as it came off the wire and the
/// facts the router learned by staging its commit.
#[derive(Clone, Debug)]
pub(crate) struct BufferedCandidate {
    pub(crate) hash: CommitHash,
    pub(crate) envelope: CommitCandidate,
    /// `None` for our own candidate: we never stage what we built. Its facts
    /// are the envelope's `proposal_count` and the batch in `pending_build`.
    pub(crate) facts: Option<StagedFacts>,
    pub(crate) is_local: bool,
}

impl BufferedCandidate {
    /// The commit's author. The MLS-authenticated sender for a remote
    /// candidate; `own` for ours.
    pub(crate) fn author(&self, own: &[u8]) -> Vec<u8> {
        match self.facts.as_ref() {
            Some(facts) => facts.sender.as_bytes().to_vec(),
            None => own.to_vec(),
        }
    }
}

/// The candidates collected for one epoch, plus the envelopes admitted for
/// staging whose facts have not come back yet. A candidate for a different
/// epoch starts a fresh round: the previous one's entries are stale.
#[derive(Clone, Debug, Default)]
pub(crate) struct CommitRoundBuffer {
    epoch: u64,
    candidates: Vec<BufferedCandidate>,
    /// Admitted envelopes waiting for the router's `handle_candidate`.
    awaiting: Vec<(CommitHash, CommitCandidate)>,
}

impl CommitRoundBuffer {
    /// Point the buffer at `epoch`, clearing a previous round's entries.
    fn open(&mut self, epoch: u64) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.candidates.clear();
            self.awaiting.clear();
        }
    }

    /// Whether `hash` is buffered or awaiting facts for `epoch`.
    pub(crate) fn knows(&self, epoch: u64, hash: &CommitHash) -> bool {
        self.epoch == epoch
            && (self.candidates.iter().any(|c| c.hash == *hash)
                || self.awaiting.iter().any(|(h, _)| h == hash))
    }

    /// Candidates plus outstanding admissions, against which the per-round
    /// one-per-member cap is measured.
    pub(crate) fn occupancy(&self, epoch: u64) -> usize {
        if self.epoch != epoch {
            return 0;
        }
        self.candidates.len() + self.awaiting.len()
    }

    /// Remember an admitted envelope until its facts arrive.
    pub(crate) fn await_facts(&mut self, epoch: u64, hash: CommitHash, envelope: CommitCandidate) {
        self.open(epoch);
        self.awaiting.push((hash, envelope));
    }

    /// Take the envelope admitted under `hash`.
    pub(crate) fn take_awaited(&mut self, hash: &CommitHash) -> Option<CommitCandidate> {
        let at = self.awaiting.iter().position(|(h, _)| h == hash)?;
        Some(self.awaiting.remove(at).1)
    }

    /// Buffer a candidate for `epoch`, deduping by hash and capping at `max`
    /// (one per member; beyond that a distinct candidate is a fork).
    pub(crate) fn insert(&mut self, epoch: u64, candidate: BufferedCandidate, max: usize) -> bool {
        self.open(epoch);
        if self.candidates.iter().any(|c| c.hash == candidate.hash) {
            return false;
        }
        if !candidate.is_local && self.candidates.len() >= max {
            return false;
        }
        self.candidates.push(candidate);
        true
    }

    /// The candidates collected for `epoch`.
    pub(crate) fn candidates(&self, epoch: u64) -> &[BufferedCandidate] {
        if self.epoch != epoch {
            return &[];
        }
        &self.candidates
    }

    /// Every buffered hash, for the discard that ends a round.
    pub(crate) fn hashes(&self, epoch: u64) -> Vec<CommitHash> {
        self.candidates(epoch).iter().map(|c| c.hash).collect()
    }

    /// Drop one candidate: the router could not merge it.
    pub(crate) fn remove(&mut self, hash: &CommitHash) {
        self.candidates.retain(|c| c.hash != *hash);
    }

    pub(crate) fn clear(&mut self) {
        self.candidates.clear();
        self.awaiting.clear();
    }
}
