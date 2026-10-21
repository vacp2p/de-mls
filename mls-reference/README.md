# Driving de-mls over an MLS group

## 0. What de-mls is

de-mls decides what happens to a group: which proposals pass, who the epoch
steward is, which commit each epoch merges, and how members are scored. It
runs the votes, elects and rotates the stewards, and runs the commit round.

It never touches MLS. No ciphertext, no key packages, no group: the engine
sees the group through three traits the application implements, takes facts
in, and returns decisions. The epoch moves only when the application executes
a `Merge` decision on its own group.

Validation is the application's. The group validates key packages, updates
and commits when it processes them, and the engine must be told when it
refuses something, so the engine can drop that work and carry on with the
rest.

The rest of this document is how the engine is driven, in the order an
integrator meets it: the traits (1), what goes in (2), what comes out (3),
the events (4), what is reported back (5), the MLS group settings the engine
assumes (6), and a worked example with an OpenMLS implementation (7). Each
section ends with its rules; every rule is a MUST, and the appendix names the
scenario in `tests/` that fails without it.

## 1. The traits you implement

Three traits, all in `de_mls::traits`. They are the only application code the
engine calls, and each is a read or a send.

**`GroupView`**: `conversation_id()`, `own_id()`, `epoch()`, `members()`, read
from the group when asked, never cached. The engine reads it at construction
and inside `commit_applied`, at no other time. A member id is the leaf's
signature key: unique within a group by MLS, identical on every node, stable
while the member keeps its key, and already carried by the joiner's key
package, so a member has the same id before and after it is seated.

**`EngineStore`**: `get(key)` and `put(key, bytes)` in a namespace of the
engine's own. The engine writes a call's state before it publishes that
call's messages, so a node never sends a vote it can forget.

**`Publish`**: `publish(bytes)` seals one control message at the current
epoch and sends it to the group. Every driving call and constructor takes a
`Publish` as its last argument (left out of the signatures below) and hands
each message to it before the call returns; nothing waits for the `Output`.

Rules:

- The conversation id MUST be the MLS group id bytes.
- A member id MUST be the leaf's signature key.
- A `put` MUST be durable when it returns.
- Exactly one live group instance per storage scope: two instances over one
  store fork the group.

## 2. What you feed in

One engine per conversation, driven from one thread at a time; calls never
interleave. Every driving call takes `now: Timestamp` first, the
application's clock, monotonic per conversation. The constructors take the
same clock, and a deadline armed at construction is measured against it.

**Control messages.** The application opens every sealed frame itself. Chat
stays with the application. A control payload goes to
`handle_control(now, sender, epoch, bytes)` with the sender and the epoch the
frame was sealed at taken from the MLS framing, never from the payload. The
application never reads, interprets or generates control bytes. A frame the
group cannot open never reaches the engine, and the engine cannot make sense
of one: whether it was garbage, a fork, or a message this node has no key
for yet, what to do with it is the application's concern, as is delivering
every message to every member.

**Commits.** A commit is MLS traffic, not a control message: the
application broadcasts the commit bytes it built and receives the ones other
members built, on whatever transport it uses for the group; de-mls has no
envelope for them. A received commit is staged on the group, never applied,
and what staging revealed goes to `handle_candidate(now, hash, facts)`:

```rust
StagedFacts {
    sender: MemberId,       // the committer, from the MLS signature on the commit
    epoch: u64,             // the epoch the commit was built at
    actions: Vec<Action>,   // what it does, by member id, in commit order
    self_removed: bool,     // whether it removes us
}
```

The actions are read off the staged commit's proposals: an Add names the
member by the signature key in its key package and carries the key package
bytes, a Remove names the member sitting at the removed leaf, an Update names
its proposer. A commit carrying any other proposal kind is not reported. The
application never ranks or validates candidates: the group rejects a foreign
or stale commit at staging, and the engine rejects every commit but the
epoch steward's.

The engine knows a commit only by its hash. `hash` is `CommitHash::of(bytes)`,
SHA-256 over the serialized commit, so every node computes the same name
from the same bytes. The engine answers with that name (`Merge { hash }`,
`Discard { hashes }`), the application finds the staged commit by it, and
`commit_applied(hash)` is how the engine tells a merge it decided from one it
did not. A staged commit is therefore held by hash until a decision names it;
the same bytes stage once.

**Refused inputs.** Traffic arrives in any order: a vote before its
proposal, a commit before the node has a steward list. The engine keeps no
such input waiting. It returns it with `Err(ConversationError::Refused(_))`,
meaning "not yet": `UnknownSession` for a vote whose proposal has not
arrived, `Syncing` for a commit the node cannot judge. `Ok` means consumed
for good, accepted or dropped. The application keeps a refused input and
feeds it again after every `Output` it drives, a vote for
`consensus_timeout`, a commit until its epoch merges, and bounds what it
keeps per sender.

**Catch-up.** A member that was away has to reach the decisions the group
took without it, and the engine has only one way to reach a decision: the
same calls, in the same order, at the same times. So catch-up is the live
path replayed. The application fetches the frames the group published since
the last one this node consumed, in send order, opens the engine at the time
of that last frame, and tells it where the present is with
`replay_until(present)`. Then it feeds each frame through the ordinary call
with the frame's own time as `now`, running `tick` for every wakeup that
fell due before that frame, and after the last frame the wakeups up to the
present. While `now` is before the present the engine is a spectator: it
casts no vote, asks for nothing, publishes nothing, and the application does
not execute `BuildCommit`, because the group already settled those. Commits
still merge only through `Merge`, exactly as live, and a member removed while
away gets `Leave` at that epoch. A frame the engine refused is still owed:
the cursor stays at the oldest refused frame until it is accepted or expires,
so a restart replays it rather than losing it; the frames after it are
replayed twice, which changes nothing.

Rules:

- One engine per conversation, one thread; `now` monotonic.
- Chat never reaches the engine. A control payload reaches it only with the
  sender MLS-authenticated and the epoch it was sealed at; one the
  application could not authenticate never does.
- A commit is staged, never applied, before `handle_candidate`; the same
  bytes stage once; the hash is one function on the bytes.
- A refused input is held and fed again after every `Output`; a candidate
  until its epoch merges, a payload for `consensus_timeout`.
- Catch-up replays the frames in send order with their own time as `now`,
  after `replay_until(present)`.

## 3. What the engine answers

Every driving call returns `Result<Output, ConversationError>`:

```rust
Output {
    decisions: Vec<Decision>,   // execute against the group, in order
    events: Vec<Event>,         // observe, section 4
    wakeup: Option<Duration>,   // call tick(now) no later than this
}
```

The decisions, each a question answered:

| decision | do |
|---|---|
| `BuildCommit { actions }` | this node is the epoch steward: build a commit carrying exactly `actions`, keep it pending, broadcast the commit bytes, report it through `handle_candidate` with the facts the build returned. The welcome carries nothing but the MLS welcome. A build the group refuses is reported through `decision_failed`, section 5 |
| `Merge { hash }` | merge the commit named by `hash`, our own pending one or a staged one; a loser clears its own pending commit first. Then `commit_applied`, section 5 |
| `Discard { hashes }` | drop these staged commits |
| `Leave` | this member is removed: tear the group down, drop the engine |

Only the application can build a commit: it is an MLS object, and the engine
has no group, no signer and no key packages. What the engine knows is the
timing. Every member expects the epoch steward's commit within
`freeze_duration` of the round opening, and that is how a silent steward is
noticed. So `BuildCommit` says "you are the steward, the round is open, this
is what goes in"; the application builds it when it is ready, inside that
window.

An `Output` is executed completely before the next driving call: decisions
in the order given, then events. A report call made while executing a
decision returns its own `Output`, executed the same way, until one is empty.
Then the wakeup is armed; `tick(now)` is the call that runs it, and it
returns an `Output` like any other.

**The questions you ask.** The local actions, each returning an `Output`:

| call | what it files |
|---|---|
| `propose_add(now, member, key_package)` | an invite. The announcement arrived outside the group; the application reads the joiner's id from the key package, SHOULD validate it, decides whether it wants the member, and files. Any member may propose; a proposal for a member already covered by another proposal or an approval is dropped |
| `propose_update(now, bytes)` | our own leaf update, built on the group first; kept and filed again after a refusal |
| `propose_remove(now, member)` | a removal |
| `propose_score_removal(now, member)` | a removal on scoring grounds |
| `vote(now, proposal_id, approve)` | this member's vote on a `VoteRequested`; silence votes automatically after `voting_delay` |
| `leave(now)` | our own departure |
| `request_sync(now)` | a sync ask, in any phase, on the application's clock |
| `request_recovery(now)` | the vote to skip a silent epoch steward |
| `propose_election(now)` | a steward election |

The epoch steward is the only member that builds a commit; a round closes
after `freeze_duration` and the steward's commit merges. Proposals and votes
are accepted in every phase; approved work lands in a later commit.

Rules:

- Every `Output` is executed completely, decisions in order, then events,
  looping on the outputs of report calls until one is empty.
- `tick(now)` is called no later than the `wakeup` of the last `Output`.
- The epoch advances only by executing `Merge`: no self-update, add,
  remove, external commit or merge of a pending commit outside one.
- After a merge every remaining staged commit is dropped as stale.

## 4. Events

What needs the application's action:

| event | what to do |
|---|---|
| `VoteRequested { proposal_id, request }` | vote. For an invite the request carries the joiner's id and key package bytes; the application may validate them (full validation plus identity equals the claimed id) before voting YES |
| `UpdateProposed { member, update }` | a peer's leaf update, approved on arrival: process it on the group and hold it, since a commit carries a peer's update by reference and every member must hold it before that commit can be built or staged |
| `CommitMissing { epoch, steward }` | the round closed with no valid commit from the epoch steward. Check whether someone in the group has it first (a member that has it sends the bytes again, and they enter as any commit); call `request_recovery` only if nobody does |
| `CandidateRejected { sender, reason }` | a stale list and a misbehaving committer look the same from here; `request_sync` settles it. A candidate rejected as not the epoch steward's stays staged until the epoch merges: after `StewardSkipped` or `SyncApplied` the application reports its facts again, since the sender may have become the epoch steward. A candidate rejected for its actions is never kept |
| `ActionsDropped { actions }` | the group refused these approved actions at the build and the epoch steward withdrew them; they land in no commit. Whoever still wants one files it again with fresh material |

What the application observes:

| event | meaning |
|---|---|
| `ConsensusReached { proposal_id, verdict, timestamp }` | a vote ended. `Rejected` is the group's decision: do not file the same request again this epoch. `Failed` is no decision (the timeout passed with neither side at threshold): filing again is the application's call, and the engine treats the new filing like a first one. `Approved` work lands in a later commit |
| `MembersChanged { added, removed }` | what a merged commit did to the member set |
| `MemberUpdated { member }` | a merged commit replaced `member`'s leaf. An update that did not land is gone with its epoch: the owner files a fresh one |
| `PhaseChange(phase)` | `Working`, `Freezing` (a round is open), `Selection` (the commit is being merged), `Syncing` (section 5) |
| `SyncApplied` | a sync answer was adopted and its steward list installed |
| `StewardSkipped { epoch, steward }` | the recovery vote passed; the next eligible steward builds |
| `CommitRoundProgress { received, expected }` | candidates seen this round |
| `MemberScoreChanged { member, previous, score }` | a fact, not a verdict; acting on it is the application's policy |
| `Error { operation, message }` | a step the engine ran on its own failed; it carries on |

Rules:

- A peer's update handed over as `UpdateProposed` is processed on the group
  and held until a merge.
- A candidate rejected as not the epoch steward's is kept staged and reported
  again after `StewardSkipped` or `SyncApplied`.
- On `CommitMissing` the application looks for the steward's commit before
  it calls `request_recovery`.

## 5. What you report back

**After a merge.** `commit_applied(now, hash, group)` is called before any
other engine call, one call per merged commit, in merge order. The engine
reads the new epoch and member set from `group`, its `GroupView`, inside that
call and at no other time; its picture of the group changes nowhere else.

**A decision that failed.** `decision_failed(now, DecisionFailure {
decision, reason, rejected })` before any other engine call. For a `Merge`
the engine closes the round and moves on. For a `BuildCommit`, `rejected`
names the members whose key package or update the group refused: the engine
drops their approved work, tells every member (`ActionsDropped` on each), and
builds again with the rest in the same output. `rejected` is empty only when
the failure was not about an action; nothing is dropped then.

**Construction.** `Engine::create(now, group, config, store)` once, right
after creating the group, over an empty store. `Engine::open(now, group,
config, store)` on every other path: on restart the application loads the
group first, then opens the engine over its store; on join it opens the
welcome first, then the same `Engine::open` over a store never written.
Either way the returned `Output` is executed.

**Sync.** A joiner, a store the engine never wrote, and a store from another
epoch all start in `Phase::Syncing`: the engine holds no steward list that
covers the epoch, refuses candidates until it adopts a `ConversationSync` or
an election lands, and asks once, during the construction call. It keeps no
deadline; every other ask is `request_sync`: while still not synced, after an
offline stretch, after `CandidateRejected`. A synced steward answers with its
list, timing, scores, approved work and open votes. A node adopts an answer
whose list is as new as its own or newer, in any phase, and reports
`SyncApplied`; an older one is ignored. What the answering steward never saw,
nobody brings: a node that missed such a proposal rejects the commit and
stays at its epoch, and heals through the missing commit, not through a
sync.

**A merge the engine did not decide.** If `commit_applied` names a hash no
`Merge` asked for, the engine does not refuse it: the group moved, and the
engine follows the group. It takes the new epoch and member set as fact,
judges nothing, and asks the stewards for a sync in the same `Output`, since
it did not see the round that produced that commit.

Rules:

- `commit_applied` after every merge, before any other engine call, one per
  commit, in merge order; the engine reads the group only there.
- A decision that could not be executed is reported with `decision_failed`
  before any other engine call; a refused build names the members refused.
- The group is loaded (or the welcome opened) before the engine is opened
  over its store.
- The engine asks for a sync once; every other ask is the application's.

## 6. Group configuration

The MLS settings the engine's timing assumes, pinned on both construction
paths:

- Wire format is pure ciphertext for application messages; commits may be
  public or private, the candidate envelope carries them in the clear either
  way.
- `max_past_epochs` is at least the commit-round depth (3) so control
  messages sealed just before a merge still open. `out_of_order_tolerance`
  and `maximum_forward_distance` allow gossip reordering (reference: 64 /
  1000).
- Ratchet-tree extension is on; welcomes carry the tree.
- Padding, ciphersuite, capabilities, group-context extensions are the
  application's.

`src/group_config.rs` pins these, stamped onto the application's builder last
so they always win.

## 7. Reference

This crate is a worked example, not a dependency: de-mls does not depend on
it, and it does not depend on de-mls.

- `src/contract.rs`: `GroupOps`, the group operations the loop below calls,
  one method per operation with its requirement in the doc comment. How
  the reference organises the group; an application organises its own
  group code however it likes, as long as the rules above hold.
- `src/reference.rs`: an OpenMLS 0.8.1 implementation of `GroupOps`. It
  owns its provider and signer; an application sharing one provider across
  conversations passes a handle to it and keeps one live instance per
  storage scope. Copy it, fold it into your group code, replace what does
  not fit.
- `src/group_config.rs`: the pins of section 6.
- `tests/conformance.rs`: the reference's own suite, written against
  `GroupOps` through a `Suite` trait, so another implementation of the same
  trait can run it by writing a second `Suite`.

### 7.1 The loop

One per conversation. Error handling and the store are elided.

```rust
struct Conversation {
    mls: MlsService,                       // the group, implements `GroupOps` and `GroupView`
    engine: Engine<KvStore>,
    publisher: Sender,                     // implements `Publish`: seals at the current epoch, sends
    held: Vec<Held>,                       // refused inputs, fed again
    welcome: Option<(CommitHash, Vec<u8>)>,// our pending commit's welcome, delivered after its merge
    update: Option<LeafParams>,            // own leaf change filed, not yet seen landing
}

enum Held {
    Candidate { hash: CommitHash, facts: StagedFacts },
    Control { arrived: Timestamp, sender: MemberId, epoch: u64, bytes: Vec<u8> },
}

impl Conversation {
    fn on_frame(&mut self, now: Timestamp, frame: Frame) {
        let out = match frame {
            Frame::Sealed(bytes) => {
                let Some(opened) = self.mls.open(&bytes) else { return };
                match opened.kind {
                    Kind::Chat    => { self.ui.show(opened); return }
                    Kind::Control => match self.engine
                        .handle_control(now, opened.sender.clone(), opened.epoch, &opened.plaintext, &mut self.publisher)
                    {
                        Err(ConversationError::Refused(_)) => {
                            self.held.push(Held::Control {
                                arrived: now, sender: opened.sender, epoch: opened.epoch,
                                bytes: opened.plaintext,
                            });
                            return
                        }
                        out => out,
                    },
                }
            }
            Frame::Commit(bytes) => {
                let hash = CommitHash::of(&bytes);
                let Ok(facts) = self.mls.stage(&bytes) else { return };   // authenticates; the same bytes stage once
                match self.engine.handle_candidate(now, hash, facts.clone(), &mut self.publisher) {
                    Err(ConversationError::Refused(_)) => {
                        self.held.push(Held::Candidate { hash, facts });
                        return
                    }
                    out => out,
                }
            }
        };
        self.drive(now, out);
    }

    fn on_wakeup(&mut self, now: Timestamp) {
        let out = self.engine.tick(now, &mut self.publisher);
        self.drive(now, out);
    }

    // The announcement arrived outside the group; the application checks it
    // and proposes.
    fn on_announcement(&mut self, now: Timestamp, key_package: Vec<u8>) {
        if self.mls.validate_key_package(&key_package).is_err() { return }
        let Ok(member) = MlsService::key_package_identity(&key_package) else { return };
        if !self.ui.wants(&member) { return }
        match self.engine.propose_add(now, member, key_package, &mut self.publisher) {
            Ok(out) => self.drive(now, out),
            Err(e) => self.ui.report(e),                             // the application's to re-file
        }
    }

    // Feeds every held input once more. Accepted or dropped for good
    // (`Ok`) leaves the list; refused stays; a fault is reported and
    // dropped. A control payload older than `consensus_timeout` expires.
    fn retry_held(&mut self, now: Timestamp) -> Output {
        let mut out = Output::default();
        let ttl = self.engine.config().consensus_timeout;
        for item in std::mem::take(&mut self.held) {
            let result = match &item {
                Held::Candidate { hash, facts } =>
                    self.engine.handle_candidate(now, *hash, facts.clone(), &mut self.publisher),
                Held::Control { arrived, .. } if now - *arrived >= ttl => continue,
                Held::Control { sender, epoch, bytes, .. } =>
                    self.engine.handle_control(now, sender.clone(), *epoch, bytes, &mut self.publisher),
            };
            match result {
                Ok(o) => out.merge(o),
                Err(ConversationError::Refused(_)) => self.held.push(item),
                Err(e) => self.ui.report(e),
            }
        }
        out
    }

    // The update is wanted until `MemberUpdated` names us; a merge drops
    // the filed one, landed or not, so the next `Working` files it again.
    fn propose_update(&mut self, now: Timestamp) -> Output {
        let Some(params) = self.update.clone() else { return Output::default() };
        let Ok(bytes) = self.mls.propose_update(params) else { return Output::default() };
        match self.engine.propose_update(now, bytes, &mut self.publisher) {
            Ok(out) => out,
            Err(_) => Output::default(),                             // blocked: next `Working`
        }
    }

    fn drive(&mut self, now: Timestamp, mut out: Output) {
        loop {
            let mut next = Output::default();
            for d in out.decisions.drain(..) {
                next.merge(self.execute(now, d));                    // report calls return Output
            }
            for e in out.events.drain(..) {
                match &e {
                    Event::MemberUpdated { member } if *member == self.mls.own_id() => {
                        self.update = None;                          // it landed
                    }
                    Event::PhaseChange(Phase::Working) => {
                        next.merge(self.propose_update(now));        // dropped by a merge: file again
                    }
                    Event::UpdateProposed { update, .. } => {
                        let _ = self.mls.validate_update(update);    // held for the commit that carries it
                    }
                    _ => {}
                }
                self.ui.notify(e);                                   // the rest is section 4
            }
            next.merge(self.retry_held(now));                      // refused inputs, once more
            if let Some(d) = out.wakeup { self.timer.arm(d); }
            if next.is_empty() { break }
            out = next;
        }
    }

    fn execute(&mut self, now: Timestamp, d: Decision) -> Output {
        match d {
            Decision::BuildCommit { actions } => {
                let built = match self.mls.build_commit(&actions) {   // stays pending
                    Ok(built) => built,
                    Err(BuildError::Refused(refused)) => {            // by member: the engine drops their work
                        let failure = DecisionFailure {
                            decision: d,
                            reason: refused.iter().map(|r| r.reason.to_string()).collect::<Vec<_>>().join("; "),
                            rejected: refused.into_iter().map(|r| r.member).collect(),
                        };
                        return self.engine.decision_failed(now, failure, &mut self.publisher);
                    }
                    Err(BuildError::Failed(e)) => {                   // not about an action: nothing is dropped
                        let failure = DecisionFailure { decision: d, reason: e.to_string(), rejected: Vec::new() };
                        return self.engine.decision_failed(now, failure, &mut self.publisher);
                    }
                };
                if let Some(w) = built.welcome { self.welcome = Some((built.hash, w)); }
                self.broadcast_commit(&built.commit);
                self.engine.handle_candidate(now, built.hash, built.facts, &mut self.publisher)
            }
            Decision::Merge { hash } => {
                self.mls.merge(hash);                                // own pending or staged; clears the rest
                match self.welcome.take() {
                    Some((h, w)) if h == hash => self.inbox.deliver(w),
                    _ => {}
                }
                self.engine.commit_applied(now, hash, &self.mls, &mut self.publisher)
            }
            Decision::Discard { hashes } => {
                for h in hashes { self.mls.discard(h); }
                Output::default()
            }
            Decision::Leave => { self.mls.delete(); Output::default() }
        }
    }
}
```

## Appendix: why each rule is a must

The rules say what an application must do for the engine's decisions to hold
on every node, not how to write the loop. Each has a scenario in `tests/`
that fails without it, on a fake loop over a fake group with no cryptography.
The scenarios assert only what an application can observe: its own group,
its engine's events and decisions, its chats, and the network. A loop written
another way should still pass them.

| rule | without it | scenario |
|---|---|---|
| outputs executed completely; refused build reported (3, 5) | a decision executed out of order or half-way leaves the group and the engine disagreeing on the epoch | `engine_bed_smoke::creator_seats_two_joiners_and_chat_crosses` |
| chat never reaches the engine; sender authenticated (2) | a vote is counted for a sender the group never authenticated | `engine_bed_smoke::creator_seats_two_joiners_and_chat_crosses` |
| refused inputs held; catch-up by replay (2) | a member that missed the traffic cannot judge the commit; a vote delivered before its proposal is lost and the node's verdict splits from the group's | `engine_bed_catch_up::a_member_offline_through_a_commit_catches_up_by_replay`, `engine_bed_catch_up::a_failed_recovery_vote_stays_failed_on_replay`, `engine_bed_liveness::a_skipped_steward_that_comes_back_follows_the_substitute`, `engine_bed_order::votes_that_arrive_before_their_proposal_are_counted` |
| a commit is staged, never applied; a rejected candidate kept and reported again (2, 4) | a commit from anyone but the epoch steward is applied; or the substitute's commit, rejected a moment before the skip verdict, is lost and the node stays behind | `engine_bed_liveness::foreign_commit_is_rejected_and_scored`, `engine_bed_liveness::a_substitute_commit_that_beats_the_verdict_is_reported_again_after_the_skip` |
| proposals accepted in every phase (3) | an announcement filed while a round is open is held until the round closes instead of landing in the next commit | `engine_bed_flow::a_proposal_filed_during_a_round_lands_in_the_next_commit` |
| the vote decides; a refused build names its members (4, 5) | an invite lands without its vote, or a rejected one lands anyway; a refused key package blocks every later build | `engine_bed_flow::creator_adds_one_then_a_member_adds_another`, `engine_bed_membership::rejected_invite_never_lands`, `engine_bed_membership::an_expired_key_package_is_withdrawn_and_the_joiner_is_invited_again` |
| `Syncing` refuses candidates; the engine asks once (5) | a node with no list guesses the steward; at list exhaustion every node enters `Syncing`, the election lands, work resumes; a joiner that took the list from the election drops the answer to its own ask and never gets the group's timing and scores; a silent epoch steward leaves the ask unanswered until the backup takes over; a joiner rejects the commit carrying work approved before its seating | `engine_bed_membership::an_exhausted_steward_list_is_replaced_without_a_stall`, `engine_bed_flow::restart_with_a_stale_snapshot_resyncs`, `engine_bed_order::a_joiner_adopts_its_answer_after_it_voted_on_an_election`, `engine_bed_sync::backup_steward_answers_when_the_epoch_steward_is_silent`, `engine_bed_sync::a_working_member_that_asks_adopts_the_equal_answer`, `engine_bed_flow::a_session_open_at_a_seating_commit_reaches_the_new_member`, `engine_bed_flow::a_proposal_filed_after_a_seating_reaches_the_new_member_in_any_order` |
| the epoch moves only by `Merge`; staged commits dropped after a merge (3) | a foreign commit merges, or a discarded one stays staged and merges later | `engine_bed_liveness::foreign_commit_is_rejected_and_scored` |
| `commit_applied` after every merge; a merge the engine did not decide is followed (5) | the engine's member set drifts from the group's; a commit merged outside a decision forks the engine from the group instead of being followed | `engine_bed_catch_up::a_member_that_missed_only_the_commit_adopts_it_and_follows_the_next`, `engine_bed_catch_up::two_members_survivor_commits_on_its_own_authority` |
| durable `put`; group before engine (1, 5) | a restart loses the vote in flight, or loads the engine before the group and starts from the wrong epoch | `engine_bed_flow::restart_mid_vote_resumes_and_merges`, `engine_bed_flow::restart_with_a_stale_snapshot_resyncs` |

One engine per thread, monotonic `now`, one group per store and `tick` by the
wakeup are preconditions the test suite satisfies by construction. The
`CommitMissing` check has no scenario yet. Two clauses have none either: the
per-sender bound on held inputs (the bed's members are honest) and the cursor
clause of catch-up (the bed never restarts a node while it holds a payload).
