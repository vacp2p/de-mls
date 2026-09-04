//! A group with no cryptography: the member set, the epoch, and the rules
//! the MLS-service contract makes the real group enforce. The router drives
//! it exactly as it would drive the real one.

use std::collections::{BTreeSet, HashMap};

use de_mls::engine::{Action, CommitHash, MemberId, StagedFacts};
use sha2::{Digest, Sha256};

/// What a sealed frame is about, so the router can branch before touching
/// the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Chat,
    Control,
}

/// A "ciphertext": the plaintext plus what the real group would authenticate
/// and bind to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub conversation_id: String,
    pub sender: MemberId,
    pub epoch: u64,
    pub kind: Kind,
    pub plaintext: Vec<u8>,
}

/// What `open` yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub sender: MemberId,
    pub epoch: u64,
    pub kind: Kind,
    pub plaintext: Vec<u8>,
}

/// The group as a joiner sees it right after the commit that seats it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub conversation_id: String,
    pub epoch: u64,
    pub members: Vec<MemberId>,
    pub authenticator: [u8; 32],
}

/// A welcome draft: who it admits and what they will see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeDraft {
    pub joiners: Vec<MemberId>,
    pub snapshot: Snapshot,
}

/// What `build_commit` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub hash: CommitHash,
    pub proposal_count: u32,
    pub commit: Vec<u8>,
    pub welcome: Option<WelcomeDraft>,
}

/// What `merge` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub epoch: u64,
    pub members: Vec<MemberId>,
}

/// Past-epoch window the contract pins (`max_past_epochs`).
pub const PAST_WINDOW: u64 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Commit {
    conversation_id: String,
    epoch: u64,
    seq: u64,
    sender: MemberId,
    actions: Vec<Action>,
}

impl Commit {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_bytes(&mut out, self.conversation_id.as_bytes());
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        put_bytes(&mut out, self.sender.as_bytes());
        out.extend_from_slice(&(self.actions.len() as u32).to_be_bytes());
        for action in &self.actions {
            match action {
                Action::Add {
                    member,
                    key_package,
                } => {
                    out.push(0);
                    put_bytes(&mut out, member.as_bytes());
                    put_bytes(&mut out, key_package);
                }
                Action::Remove { member } => {
                    out.push(1);
                    put_bytes(&mut out, member.as_bytes());
                }
            }
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut cursor = Cursor { bytes, at: 0 };
        let conversation_id = String::from_utf8(cursor.bytes()?).ok()?;
        let epoch = cursor.u64()?;
        let seq = cursor.u64()?;
        let sender = MemberId::from(cursor.bytes()?);
        let count = cursor.u32()? as usize;
        let mut actions = Vec::with_capacity(count);
        for _ in 0..count {
            let tag = cursor.u8()?;
            let member = MemberId::from(cursor.bytes()?);
            actions.push(match tag {
                0 => Action::Add {
                    member,
                    key_package: cursor.bytes()?,
                },
                1 => Action::Remove { member },
                _ => return None,
            });
        }
        Some(Self {
            conversation_id,
            epoch,
            seq,
            sender,
            actions,
        })
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let len = self.u32()? as usize;
        self.take(len).map(<[u8]>::to_vec)
    }
}

/// One node's group.
#[derive(Debug)]
pub struct FakeMls {
    conversation_id: String,
    own: MemberId,
    epoch: u64,
    join_epoch: u64,
    members: BTreeSet<MemberId>,
    authenticator: [u8; 32],
    active: bool,
    seq: u64,
    pending: Option<(CommitHash, Commit)>,
    staged: HashMap<CommitHash, Commit>,
}

impl FakeMls {
    /// The identity a name maps to; the "signature key".
    pub fn member_id(name: &str) -> MemberId {
        MemberId::from(format!("key:{name}").into_bytes())
    }

    /// The key package a name mints; `key_package_identity` reads it back.
    pub fn key_package(name: &str) -> Vec<u8> {
        format!("kp:{name}").into_bytes()
    }

    /// Parse only, no validation: the joiner's future member id.
    pub fn key_package_identity(bytes: &[u8]) -> Option<MemberId> {
        let text = std::str::from_utf8(bytes).ok()?;
        let name = text.strip_prefix("kp:")?;
        Some(Self::member_id(name))
    }

    /// Creator side: a one-member group at epoch 0.
    pub fn create(conversation_id: &str, own: MemberId) -> Self {
        Self {
            conversation_id: conversation_id.to_string(),
            epoch: 0,
            join_epoch: 0,
            members: BTreeSet::from([own.clone()]),
            own,
            authenticator: [0; 32],
            active: true,
            seq: 0,
            pending: None,
            staged: HashMap::new(),
        }
    }

    /// Joiner side: adopt the snapshot a welcome carried.
    pub fn from_welcome(own: MemberId, snapshot: &Snapshot) -> Self {
        Self {
            conversation_id: snapshot.conversation_id.clone(),
            epoch: snapshot.epoch,
            join_epoch: snapshot.epoch,
            members: snapshot.members.iter().cloned().collect(),
            own,
            authenticator: snapshot.authenticator,
            active: true,
            seq: 0,
            pending: None,
            staged: HashMap::new(),
        }
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn own_id(&self) -> &MemberId {
        &self.own
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn members(&self) -> Vec<MemberId> {
        self.members.iter().cloned().collect()
    }

    /// The fake epoch authenticator: a hash chain over merged commits. Two
    /// nodes that merged the same commits agree on it.
    pub fn authenticator(&self) -> [u8; 32] {
        self.authenticator
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn seal(&self, kind: Kind, plaintext: &[u8]) -> Sealed {
        Sealed {
            conversation_id: self.conversation_id.clone(),
            sender: self.own.clone(),
            epoch: self.epoch,
            kind,
            plaintext: plaintext.to_vec(),
        }
    }

    /// The contract's window rules: wrong group, newer than current, older
    /// than the past window, older than own join, or a non-member sender all
    /// drop.
    pub fn open(&self, sealed: &Sealed) -> Option<Opened> {
        if !self.active || sealed.conversation_id != self.conversation_id {
            return None;
        }
        if sealed.epoch > self.epoch
            || sealed.epoch < self.join_epoch
            || self.epoch.saturating_sub(sealed.epoch) > PAST_WINDOW
        {
            return None;
        }
        if !self.members.contains(&sealed.sender) {
            return None;
        }
        Some(Opened {
            sender: sealed.sender.clone(),
            epoch: sealed.epoch,
            kind: sealed.kind,
            plaintext: sealed.plaintext.clone(),
        })
    }

    /// Build a commit for the current epoch and keep it pending. Removes of
    /// non-members are skipped, as the contract requires.
    pub fn build_commit(&mut self, actions: &[Action]) -> Built {
        let actions: Vec<Action> = actions
            .iter()
            .filter(|a| match a {
                Action::Remove { member } => self.members.contains(member),
                Action::Add { .. } => true,
            })
            .cloned()
            .collect();
        self.seq += 1;
        let commit = Commit {
            conversation_id: self.conversation_id.clone(),
            epoch: self.epoch,
            seq: self.seq,
            sender: self.own.clone(),
            actions,
        };
        let bytes = commit.encode();
        let hash = CommitHash::of(&bytes);
        let joiners: Vec<MemberId> = commit
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Add { member, .. } => Some(member.clone()),
                Action::Remove { .. } => None,
            })
            .collect();
        let welcome = (!joiners.is_empty()).then(|| WelcomeDraft {
            joiners,
            snapshot: self.snapshot_after(&commit, hash),
        });
        let proposal_count = commit.actions.len() as u32;
        self.pending = Some((hash, commit));
        Built {
            hash,
            proposal_count,
            commit: bytes,
            welcome,
        }
    }

    pub fn pending_hash(&self) -> Option<CommitHash> {
        self.pending.as_ref().map(|(h, _)| *h)
    }

    /// Stage a remote commit without applying it. Several may be held.
    pub fn stage(&mut self, bytes: &[u8]) -> Result<StagedFacts, String> {
        let commit = Commit::decode(bytes).ok_or("malformed commit")?;
        if commit.conversation_id != self.conversation_id {
            return Err("group id mismatch".into());
        }
        if commit.epoch < self.epoch {
            return Err("past epoch".into());
        }
        if commit.epoch > self.epoch {
            return Err("future epoch".into());
        }
        if !self.members.contains(&commit.sender) {
            return Err("sender is not a member".into());
        }
        let facts = StagedFacts {
            sender: commit.sender.clone(),
            epoch: commit.epoch,
            actions: commit.actions.clone(),
            proposal_count: commit.actions.len() as u32,
            self_removed: commit
                .actions
                .iter()
                .any(|a| matches!(a, Action::Remove { member } if *member == self.own)),
        };
        self.staged.insert(CommitHash::of(bytes), commit);
        Ok(facts)
    }

    /// Merge the pending own commit or a staged one. Merging a remote clears
    /// the pending own commit first; every other staged commit is then stale.
    pub fn merge(&mut self, hash: CommitHash) -> Result<Applied, String> {
        let commit = match self.pending.take() {
            Some((h, c)) if h == hash => c,
            other => {
                self.pending = None;
                let _ = other;
                self.staged.remove(&hash).ok_or("no such staged commit")?
            }
        };
        self.staged.clear();
        self.apply(&commit, hash);
        Ok(Applied {
            epoch: self.epoch,
            members: self.members(),
        })
    }

    pub fn discard(&mut self, hash: CommitHash) {
        self.staged.remove(&hash);
    }

    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    pub fn staged_count(&self) -> usize {
        self.staged.len()
    }

    pub fn delete(&mut self) {
        self.active = false;
        self.pending = None;
        self.staged.clear();
    }

    fn apply(&mut self, commit: &Commit, hash: CommitHash) {
        for action in &commit.actions {
            match action {
                Action::Add { member, .. } => {
                    self.members.insert(member.clone());
                }
                Action::Remove { member } => {
                    self.members.remove(member);
                    if *member == self.own {
                        self.active = false;
                    }
                }
            }
        }
        self.epoch += 1;
        self.authenticator = chain(self.authenticator, hash);
    }

    fn snapshot_after(&self, commit: &Commit, hash: CommitHash) -> Snapshot {
        let mut members = self.members.clone();
        for action in &commit.actions {
            match action {
                Action::Add { member, .. } => {
                    members.insert(member.clone());
                }
                Action::Remove { member } => {
                    members.remove(member);
                }
            }
        }
        Snapshot {
            conversation_id: self.conversation_id.clone(),
            epoch: self.epoch + 1,
            members: members.into_iter().collect(),
            authenticator: chain(self.authenticator, hash),
        }
    }
}

fn chain(previous: [u8; 32], hash: CommitHash) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(previous);
    hasher.update(hash.as_bytes());
    hasher.finalize().into()
}
