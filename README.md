# De-MLS

[![Crates.io](https://img.shields.io/crates/v/de-mls.svg)](https://crates.io/crates/de-mls)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Decentralized MLS — end-to-end encrypted group messaging with consensus-based
membership over gossipsub-like networks. de-mls implements the [Decentralized MLS Off-Chain Consensus](https://github.com/logos-co/logos-lips/blob/master/docs/anoncomms/raw/decentralized-mls-offchain-consensus.md)
protocol on top of [OpenMLS](https://github.com/openmls/openmls): MLS
cryptography for the secure channel, and a hashgraph-like consensus service for
proposal voting and steward election.

The library's product is a single per-conversation handle — `Conversation` —
modeled on OpenMLS's `MlsGroup`. It owns every protocol decision (MLS encryption,
proposal voting, steward commits, freeze timing); transport and identity stay on
your side of the boundary. It runs synchronously and is generic over its
consensus plug-in and its peer-score storage backend.

> Looking for a runnable app? An example integration — a gateway, a Waku
> delivery service, and a Dioxus desktop client wired onto this library — lives
> in the **[de-mls-poc](https://github.com/vacp2p/de-mls-poc)** repository.

## What you own vs. what de-mls owns

**You provide:** identity (opaque member-id bytes and the map from a member to
its transport address), the transport itself, the OpenMLS provider (crypto +
storage), the consensus backend (proposal/vote storage + a vote-signing key),
key-package minting, and the registry of conversations.

**de-mls owns:** the protocol — MLS commits, proposal voting, steward election —
along with the per-conversation state behind it: proposal queues, deduplication,
the steward list, peer scores, and the `Conversation` state machine. It also
owns the **agreement/settle timing** that every member must share (see
[Driving & timing](#driving--timing)).

**You also own the liveness timing:** de-mls keeps no liveness timers. It tells
you *what* is actionable (via condition queries) and *how* to act (via
triggers); *when* to act — commit, take over a silent steward, recover — is
yours to drive, on a clock or by hand.

## The `Conversation` API

```rust,ignore
use de_mls::Conversation;
use de_mls::defaults::{DefaultConsensusPlugin, InMemoryPeerScoreStorage};

// The first generic is the consensus backend, the second the peer-score
// *storage* backend. `consensus` is a `ConsensusPlugin` instance you hold and
// pass by reference; `scoring` is a `PeerScoringService` built over the storage
// (see `de_mls::defaults::DefaultPeerScoring`). The steward roster is
// library-owned — you set its size bounds on `config` (a `ConversationConfig`).
// The third generic is the time source: a `WallClock` impl you provide —
// wrap `SystemTime` in production, use `MockClock` for virtual-time tests.
// Create a conversation you steward, or join one from a welcome:
let mut convo: Conversation<DefaultConsensusPlugin, InMemoryPeerScoreStorage, _> =
    Conversation::create(id, member_id, &provider, credential, suite, &signer,
                         &consensus, scoring, clock, app_id, config)?;

// let joined = Conversation::join(member_id, &provider, &signer,
//                                 welcome_bytes, sync_bytes, …)?;  // Ok(None) = not for us

// Drive it once per wakeup cycle, then drain its products:
convo.process_inbound(&provider, &signer, &sender, &payload)?; // feed inbound bytes
convo.poll(&provider, &signer);                                // tick timers / freeze / commits
for event in convo.drain_events()   { /* AppMessage, WelcomeReady, PhaseChange, … */ }
for out   in convo.drain_outbound() { /* publish out.payload on your transport */ }
```

The conversation is pull-based: it **buffers** outbound for you to publish, and
reports its next deadline via `next_wakeup_in()`, advancing when you call
`poll()`. Membership and chat are plain methods: `add_member` / `sponsor_member`,
`remove_member`, `vote`, `send_message`, `leave`.

Default implementations (consensus over `hashgraph-like-consensus` and
in-memory peer-score storage) live in `de_mls::defaults` — adopt them
wholesale or swap either. The steward list is library-owned; you set its size
bounds via `ConversationConfig`'s `steward_list` field.

A complete, runnable construction — creator and joiner built straight from
direct arguments — is in
[`tests/standalone_construction.rs`](tests/standalone_construction.rs).

## Driving & timing

de-mls carries no liveness timers. Each cycle the app: calls `poll()` (resolves
votes, advances any in-flight commit round, and drives reelection internally),
reads the **condition queries** to see what's actionable, and pulls the matching
**trigger** when its own timer or signal says to. The full surface is indexed in
[`src/conversation/driving.rs`](src/conversation/driving.rs):

| Condition query              | Trigger                      | Meaning                                            |
| ---------------------------- | ---------------------------- | -------------------------------------------------- |
| `pending_commit_work()`      | `commit_now()`               | approved batch waiting to commit                   |
| `pending_buffered_updates()` | `propose_buffered_updates()` | buffered joins/removes to propose                  |
| `pending_sync_resend()`      | `share_conversation_sync()`  | unanswered sync request (backup)                   |
| `ReelectionExhausted` event  | `request_recovery()`         | open Layer-3 when reelection can't elect a steward |
| *(recovery open)*            | `commit_in_recovery()`       | mint in Layer-3 recovery                           |
| *(commit dropped)*           | `resend_commit()`            | re-broadcast a held candidate                      |

The triggers **self-gate** — each is a no-op when there's nothing to do — so the
simplest integrator just calls the trigger every cycle. The paired query is an
optional *peek* (observe without acting): use it to run a delay before the
trigger — e.g. a commit-inactivity window that batches proposals into one commit —
or to drive a UI.

**Reelection is de-mls's, not yours.** A silent reelection round advances
internally on `poll()` — the retry round re-seeds the shared steward list, so it
must move in lockstep. Once it exhausts `max_reelection_attempts`, de-mls emits
`ReelectionExhausted` and *you* decide whether to open Layer-3 recovery.

**Two kinds of timing.** de-mls owns the **agreement/settle** durations on
`ConversationConfig` — every member must agree on them, so they ride in
`ConversationSync` to joiners:

- `voting_delay` — grace for a manual vote before the auto-vote fires (steward elections auto-validate with no delay)
- `consensus_timeout` — how long a vote session stays open, and the silent-reelection-round window
- `freeze_duration` — commit-round candidate-collection window (reused during recovery)
- `proposal_expiration`
- `max_reelection_attempts` — retry cap before Layer-3 escalation

The **liveness** durations — how long *you* wait before driving `commit_now`, a
backup takeover, or recovery — are yours; keep them in your own config.

**How they depend on each other.** Respect, per member:

``` text
voting_delay  <  consensus_timeout  <  your commit-inactivity  <  your takeover window
```

and, across the network, `consensus_timeout > Δ` (max message delay): if a vote
session times out before votes propagate, it resolves on the silent-vote
fallback, which can resolve differently per node and split the steward list.

## Consensus

Membership changes are agreed by vote before they commit, and de-mls owns that
orchestration end to end: opening a proposal, collecting votes, the auto-vote
and timeout deadlines, and turning a resolved decision into the next steward
commit, freeze, or election.

You supply a `ConsensusPlugin` — the consensus backend, which is two things:
where proposals and votes are stored, and the key that signs votes. The
conversation id serves as the consensus scope, and outcome delivery and
per-conversation session capacity are library-owned, so the backend stays that
small. One backend instance backs all of a member's conversations; you hand it
to each by reference.

`de_mls::defaults::DefaultConsensusPlugin` runs the `hashgraph-like-consensus`
library over an in-memory store and an Ethereum vote signer — build it with
`DefaultConsensusPlugin::new(signer)`. A durable integrator keeps the same
shape and swaps the store for one backed by a database.

## Peer scoring

de-mls owns the peer-scoring protocol: it turns observed events into score
deltas, evaluates scores against the removal threshold, and drives
`SCORE_BELOW_THRESHOLD` removals through consensus. You supply only two
things — a `PeerScoreStorage` backend (the per-conversation member→score
table) and a `ScoringConfig` (per-event deltas, default score, threshold) —
which are combined into the library-owned `PeerScoringService`. There is no
scoring-behavior trait to override; score updates are a protocol decision, so
the library keeps a single implementation.

`de_mls::defaults::InMemoryPeerScoreStorage` is a ready in-memory backend; a
durable integrator can back the table with sqlite or a key-value store.
Storage methods are fallible (the trait carries an associated `Error` type),
so a durable backend surfaces I/O failures rather than swallowing a score
write.

## Steward list

Who may commit each epoch — the steward roster and its epoch/backup rotation —
is fully library-owned. You set only its size bounds (`sn_min` / `sn_max`, the
`steward_list` field on `ConversationConfig`); de-mls generates the list,
validates election proposals, runs the election through consensus, and rotates
the epoch steward.

Generation is deterministic and normative: every member derives the identical
roster by sorting on `SHA256(epoch ‖ retry_round ‖ member_id ‖
conversation_id)`, and a proposal that doesn't reproduce it is rejected by all
peers (RFC §"Steward list creation"). There is nothing to override — a
divergent generator would fork the group — so, unlike consensus and scoring,
the steward list takes no plug-in.

## Build & test

```bash
cargo build -p de-mls
cargo test  -p de-mls --release
cargo clippy -p de-mls --tests -- -D warnings
RUSTDOCFLAGS='-Dwarnings' cargo doc -p de-mls --lib --no-deps --document-private-items
```

Building requires the Rust toolchain (edition 2024) and `protoc`, which
`build.rs` uses to compile the protobuf definitions.

## Documentation

- **API:** every public trait carries its contract in rustdoc — run
  `cargo doc -p de-mls --open`.
- **Living example:** [`tests/standalone_construction.rs`](tests/standalone_construction.rs)
  builds a creator and a joiner straight from direct arguments.
- **Protocol:** de-mls follows the
  [Decentralized MLS Off-Chain Consensus](https://github.com/logos-co/logos-lips/blob/master/docs/anoncomms/raw/decentralized-mls-offchain-consensus.md)
  specification.

## Contributing

Issues and pull requests are welcome. Please include reproduction steps, relevant
logs, and test coverage where possible.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option.
