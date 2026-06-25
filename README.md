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

**de-mls owns:** the protocol — MLS commits, proposal voting, steward election,
and freeze timing — along with the per-conversation state behind it: proposal
queues, deduplication, the steward list, peer scores, and the `Conversation`
state machine.

## The `Conversation` API

```rust,ignore
use de_mls::Conversation;
use de_mls::defaults::{DefaultConsensusPlugin, InMemoryPeerScoreStorage};

// The first generic is the consensus backend, the second the peer-score
// *storage* backend. `consensus` is a `ConsensusPlugin` instance you hold and
// pass by reference; `scoring` is a `PeerScoringService` built over the storage
// (see `de_mls::defaults::DefaultPeerScoring`). The steward roster is
// library-owned — you set its size bounds on `config` (a `ConversationConfig`).
// Create a conversation you steward, or join one from a welcome:
let mut convo: Conversation<DefaultConsensusPlugin, InMemoryPeerScoreStorage> =
    Conversation::create(id, member_id, &provider, credential, suite, &signer,
                         &consensus, scoring, app_id, config)?;

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
