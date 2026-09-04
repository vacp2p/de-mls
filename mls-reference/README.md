# MLS service contract (reference)

The group operations the protocol engine delegates to the application, the
guarantees each one carries, and an OpenMLS 0.8.1 implementation that satisfies
them.

Nothing in this contract is called by the engine. Every operation is called by
the router against the application's own group-owning code. "MLS service" below
means that code, organised however the application likes. The application
already owns the group config builder, the provider and signer, key-package
minting, validation and identity, and welcome delivery; the rest is new code,
with `src/reference.rs` as the worked example. Each line is a MUST unless
marked SHOULD; the rationale is attached because the reference will be
reorganised.

## 1. Group configuration

- Wire format MUST be pure ciphertext for application messages; commits MAY be
  public or private, the candidate envelope carries them in the clear either way.
- `max_past_epochs` MUST be at least the commit-round depth (3) so control
  messages sealed just before a merge still open. `out_of_order_tolerance` and
  `maximum_forward_distance` MUST allow gossip reordering (reference: 64 / 1000).
- Ratchet-tree extension MUST be on; welcomes carry the tree.
- Padding, ciphersuite, capabilities, group-context extensions are the
  application's.

`src/group_config.rs` pins these on both construction paths, stamped onto the
application's builder last so they always win.

## 2. Operations and their guarantees

| op | returns | requirement |
|---|---|---|
| `create(config)` | group, epoch, members | group id MUST equal the conversation id bytes |
| `open_welcome(welcome)` | group, conversation id, epoch, members, or "not for me" | MUST distinguish "not addressed to me" from failure |
| `load(conversation id)` | group | |
| `members()` | `[member_id]` | member id = the leaf's signature key: unique within a group by MLS, identical on every node, stable while the member keeps its key |
| `epoch()`, `own_id()` | | read from the group, never cached |
| `seal(plaintext)` | ciphertext | MUST persist the ratchet before returning |
| `open(ciphertext)` | `{sender, epoch, plaintext}` or drop | MUST return the sender from the MLS signature, never from the payload; MUST drop group-id mismatch, epochs newer than current, older than `current - past_window`, and older than own join; MUST NOT process commit or proposal content here |
| `key_package_identity(bytes)` | member id (the leaf's signature key) | parse only; the same value the member has after seating |
| `validate_key_package(bytes)` | ok / error | full MLS validation |
| `build_commit(actions)` | `{hash, proposal_count, commit_bytes, welcome?}` | proposals MUST be inline, in the given order; MUST force a self-update; MUST validate every key package; MUST stay pending and MUST NOT merge; removes of non-members MUST be skipped and reported |
| `stage(commit_bytes)` | `{sender, epoch, actions, proposal_count, self_removed}` | MUST NOT apply; MUST hold several staged commits at once; MUST reject group-id mismatch and past epochs; `actions` lists adds and removes by member id in commit order |
| `merge(hash)` | `{epoch, members}` | own pending commit MUST be cleared before merging a remote one; MUST persist before returning |
| `discard(hash)`, `clear_pending()` | | |
| `commit_hash(bytes)` | hash | one function, same on every node |
| `delete()` | | idempotent teardown after `Decision::Leave` |
| welcome delivery | | MUST carry the bootstrap bytes the engine hands over for that commit |

## 3. Router contract

The router is application code. It is the only caller of the engine and the
only caller of the group. These rules are what make the split safe; each is a
MUST.

**Driving**

1. One engine per conversation, driven from one thread at a time. Calls
   never interleave.
2. `now` passed to the engine is monotonic per conversation. `tick(now)`
   is called no later than the `wakeup` of the last `Output`.
3. Every `Output` is executed completely before the next driving call,
   in this order: seal and send `outbound` (at the current epoch), then
   execute `decisions` in the order given, then handle `events`. A report
   call made while executing a decision returns its own `Output`, which
   is executed the same way; the router loops until an `Output` is empty.
4. A decision the router could not execute (an MLS error on merge, a
   missing staged commit) is reported with `decision_failed(now, decision,
   reason)` before any other engine call. The engine discards and moves on.

**Inbound**

5. Sealed frames are opened first. A control payload reaches the engine
   only with the sender MLS authenticated and the epoch it was sealed at.
   A payload the router could not authenticate never reaches the engine.
6. The router never reads, interprets or generates control bytes, and
   never ranks or validates candidates. A candidate on the wire is the raw
   commit bytes; the router stages it and reports the facts through
   `handle_candidate`, the group rejecting a foreign or stale commit.
7. Chat content never reaches the engine.
7a. An invite proposal carries the joiner's id as the proposer read it from
    the key package. Every member executes `Decision::ValidateKeyPackage`
    (full validation plus identity equals the claimed id) and reports
    `key_package_checked` before its vote proceeds; the engine never parses
    a key package.

**Commits**

8. The epoch advances only by executing `Decision::Merge`. No self-update,
   add, remove, external commit or merge of a pending commit outside one.
9. Staged commits are held by hash. `Decision::Discard` drops the named
   ones; after a merge every remaining staged commit is dropped as stale.
10. A loser clears its own pending commit before merging the winner.
11. After a merge, `commit_applied(now, hash, epoch, members)` is called
    before any other engine call. A missed report forks the steward
    election on that node.
12. After `Decision::BuildCommit` the router builds, keeps the commit
    pending, broadcasts the commit bytes, and reports it through
    `handle_candidate` with the facts the build returned. The welcome
    carries nothing but the MLS welcome; the joiner's sync arrives as an
    ordinary control message.

**Storage and restart**

13. The `EngineStore` write of a driving call is durable before the
    `outbound` of the same `Output` is sent. A node must not send a vote
    it can forget.
14. On restart the router loads the group first, then
    `Engine::restore(store, own, epoch, members)`, then executes the
    returned `Output` (it may contain a sync request).
15. On join the router opens the welcome first, then
    `Engine::join(store, own, epoch, members)`.
16. Exactly one live group instance per storage scope.

## 4. Reference router

Not a component we ship; an example of the loop the contract describes.
Error handling and the store are elided.

```rust
struct Router {
    mls: MlsService,                       // application-owned group
    engine: Engine<KvStore>,
    staged: HashMap<CommitHash, StagedCommit>,
}

impl Router {
    fn on_frame(&mut self, now: Timestamp, frame: Frame) {
        let out = match frame {
            Frame::Sealed(bytes) => {
                let Some(opened) = self.mls.open(&bytes) else { return };
                match opened.kind {
                    Kind::Chat    => { self.app.show(opened); return }
                    Kind::Control => self.engine
                        .handle_control(now, opened.sender, opened.epoch, &opened.plaintext),
                }
            }
            Frame::Commit(bytes) => {
                let Ok(facts) = self.mls.stage(&bytes) else { return };   // authenticates
                let hash = CommitHash::of(&bytes);
                self.staged.insert(hash, facts.staged);
                self.engine.handle_candidate(now, hash, facts)
            }
        };
        self.drive(now, out);
    }

    fn on_wakeup(&mut self, now: Timestamp) {
        let out = self.engine.tick(now);
        self.drive(now, out);
    }

    fn drive(&mut self, now: Timestamp, mut out: Output) {
        loop {
            for o in out.outbound.drain(..) { self.send(o); }       // sealed at current epoch
            let mut next = Output::default();
            for d in out.decisions.drain(..) {
                next.merge(self.execute(now, d));                    // report calls return Output
            }
            for e in out.events.drain(..) { self.app.notify(e); }
            if let Some(d) = out.wakeup { self.timer.arm(d); }
            if next.is_empty() { break }
            out = next;
        }
    }

    fn execute(&mut self, now: Timestamp, d: Decision) -> Output {
        match d {
            Decision::BuildCommit { actions } => {
                let built = self.mls.build_commit(&actions);          // stays pending
                self.broadcast_commit(&built.commit);
                self.engine.handle_candidate(now, built.hash, built.facts)
            }
            Decision::Merge { hash } => {
                let applied = if self.mls.pending_hash() == Some(hash) {
                    self.mls.merge_pending()
                } else {
                    self.mls.clear_pending();
                    let staged = self.staged.remove(&hash).expect("staged");
                    self.mls.merge_staged(staged)
                };
                self.staged.clear();                                 // the rest are stale
                if let Some(w) = self.mls.take_welcome(hash) { self.inbox.deliver(w); }
                self.engine.commit_applied(now, hash, applied.epoch, applied.members)
            }
            Decision::Discard { hashes } => {
                for h in hashes { self.staged.remove(&h); }
                Output::default()
            }
            Decision::ValidateKeyPackage { proposal_id, member, key_package } => {
                let ok = self.mls.validate_key_package(&key_package).is_ok()
                    && MlsService::key_package_identity(&key_package) == Ok(member);
                self.engine.key_package_checked(now, proposal_id, ok)
            }
            Decision::Leave => { self.mls.delete(); Output::default() }
        }
    }
}
```

## How to use this crate

Implement `GroupOps` for your MLS service. The trait is the contract table in
section 2, one method per row; the doc comment on each method carries that
row's requirement. Construction is not on the trait, because creating, joining
and loading need your provider, signer, credential and configuration — the
requirements for it are on `Reference::create`, `Reference::open_welcome` and
`Reference::load`.

Then run the conformance suite against your type. `tests/conformance.rs`
reaches an implementation only through the `Suite` trait at the top of the
file, which says how to mint a member, stand a group up, and read a group's
epoch authenticator. Write a second `Suite` impl for your service and point the
`#[test]` functions at it; every case is already generic. A green suite is what
"done" means for this contract.

`Reference` itself is a worked example rather than a dependency: copy it, fold
it into your group code, replace what does not fit. It owns its provider and
signer, because `GroupOps` takes neither — an application sharing one provider
across conversations passes a handle to it and keeps exactly one live instance
per storage scope.
