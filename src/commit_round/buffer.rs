//! Commit-round candidate buffer: the per-epoch set of commit candidates the
//! round collects before selection, and the outcome of a buffering attempt.

use crate::{
    CommitHash, NoopReason, ProcessResult, compute_commit_hash,
    protos::de_mls::messages::v1::CommitCandidate,
};

/// Commit-round candidate buffer for deterministic selection: the candidates
/// collected for `epoch`. A candidate for a different epoch starts a fresh
/// round, discarding any stragglers from the previous one.
#[derive(Clone, Debug, Default)]
pub struct CommitRoundBuffer {
    epoch: u64,
    candidates: Vec<BufferedCommitCandidate>,
}

impl CommitRoundBuffer {
    /// Buffer a candidate for `epoch`, deriving its commit hash and assembling
    /// the [`BufferedCommitCandidate`] internally. A candidate for a different
    /// epoch than the one currently held starts a fresh round (the previous
    /// candidates are stale). `max_candidates` caps the buffer at one-per-member;
    /// beyond that a distinct candidate is spoofed/forked and refused.
    pub fn add(
        &mut self,
        candidate: CommitCandidate,
        is_local_candidate: bool,
        welcome_bytes: Option<Vec<u8>>,
        joiner_identities: Vec<Vec<u8>>,
        epoch: u64,
        max_candidates: usize,
    ) -> CommitBufferOutcome {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.candidates.clear();
        }

        let commit_hash = compute_commit_hash(&candidate.commit_message);
        if self.candidates.iter().any(|c| c.commit_hash == commit_hash) {
            return CommitBufferOutcome::DuplicateHash;
        }
        if self.candidates.len() >= max_candidates {
            return CommitBufferOutcome::CapReached;
        }

        self.candidates.push(BufferedCommitCandidate {
            candidate_msg: candidate,
            commit_hash,
            is_local_candidate,
            welcome_bytes,
            joiner_identities,
        });
        CommitBufferOutcome::Buffered
    }

    /// Number of candidates buffered.
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    /// Whether the buffer already holds our own (locally-minted) candidate —
    /// used by the recovery auto-fallback to avoid minting twice.
    pub fn has_local_candidate(&self) -> bool {
        self.candidates.iter().any(|c| c.is_local_candidate)
    }

    /// Discard all buffered candidates.
    pub fn clear(&mut self) {
        self.candidates.clear();
    }

    /// Take the candidates buffered for `epoch`, leaving the buffer empty.
    /// Empty when the buffer holds a different (stale) epoch's candidates.
    pub fn take(&mut self, epoch: u64) -> Vec<BufferedCommitCandidate> {
        if self.epoch != epoch {
            return Vec::new();
        }
        std::mem::take(&mut self.candidates)
    }
}
/// A commit candidate buffered during the commit round for later selection.
#[derive(Clone, Debug)]
pub struct BufferedCommitCandidate {
    pub candidate_msg: CommitCandidate,
    pub commit_hash: CommitHash,
    pub is_local_candidate: bool,
    pub welcome_bytes: Option<Vec<u8>>,
    /// Member ids admitted by this commit's welcome (one per Add). Set only
    /// on the local candidate, where `welcome_bytes` is held; empty on remote
    /// candidates, which never carry a welcome.
    pub joiner_identities: Vec<Vec<u8>>,
}

/// Outcome of [`CommitRoundBuffer::add`]. An enum rather than `Result<(), _>`
/// because the non-success cases are well-defined protocol states (retransmit,
/// late offer, spoofed fork) that callers handle differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitBufferOutcome {
    /// Candidate stored in the buffer for this epoch.
    Buffered,
    /// A candidate with the same commit hash is already buffered.
    DuplicateHash,
    /// Buffer already holds one candidate per member; the rest are refused.
    CapReached,
}

impl CommitBufferOutcome {
    /// Map a buffering outcome to the inbound [`ProcessResult`]. `steward_id`
    /// labels the `CommitCandidateReceived` reported on a successful buffer; the
    /// refused cases surface as a `Noop` with the matching reason.
    pub fn into_process_result(self, steward_id: Vec<u8>) -> ProcessResult {
        match self {
            Self::Buffered => ProcessResult::CommitCandidateReceived { steward_id },
            Self::DuplicateHash => ProcessResult::Noop(NoopReason::DuplicateBufferedHash),
            Self::CapReached => ProcessResult::Noop(NoopReason::CandidateBufferFull),
        }
    }
}
