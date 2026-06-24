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
consensus and steward plug-ins and its peer-score storage backend.

> Looking for a runnable app? An example integration — a gateway, a Waku
> delivery service, and a Dioxus desktop client wired onto this library — lives
> in the **[de-mls-poc](https://github.com/vacp2p/de-mls-poc)** repository.

## What you own vs. what de-mls owns

**You provide:** identity (opaque member-id bytes and the map from a member to
its transport address), the transport itself, the OpenMLS provider (crypto +
storage), key-package minting, and the registry of conversations.

**de-mls owns:** the protocol — MLS commits, proposal voting, steward election,
and freeze timing — along with the per-conversation state behind it: proposal
queues, deduplication, the steward list, peer scores, and the `Conversation`
state machine.

## The `Conversation` API

```rust,ignore
use de_mls::Conversation;
use de_mls::defaults::{DefaultConsensusPlugin, DefaultStewardList, InMemoryPeerScoreStorage};

// The middle generic is the peer-score *storage* backend; `scoring` is a
// `PeerScoringService` built over it (see `de_mls::defaults::DefaultPeerScoring`).
// Create a conversation you steward, or join one from a welcome:
let mut convo: Conversation<DefaultConsensusPlugin, InMemoryPeerScoreStorage, DefaultStewardList> =
    Conversation::create(id, &provider, credential, suite, &signer,
                         scoring, steward, consensus, app_id, config, member_id)?;

// let joined = Conversation::join(&provider, welcome_bytes, sync_bytes, …, &signer)?;  // Ok(None) = not for us

// Drive it once per wakeup cycle, then drain its products:
convo.process_inbound(&provider, &sender, &payload, &signer)?; // feed inbound bytes
convo.poll(&provider, &signer);                                // tick timers / freeze / commits
for event in convo.drain_events()   { /* AppMessage, WelcomeReady, PhaseChange, … */ }
for out   in convo.drain_outbound() { /* publish out.payload on your transport */ }
```

The conversation is pull-based: it **buffers** outbound for you to publish, and
reports its next deadline via `next_wakeup_in()`, advancing when you call
`poll()`. Membership and chat are plain methods: `add_member` / `sponsor_member`,
`remove_member`, `vote`, `send_message`, `leave`.

Default plug-in implementations (consensus over `hashgraph-like-consensus`,
in-memory peer-score storage, the deterministic steward list) live in
`de_mls::defaults` — adopt them wholesale or swap any one.

A complete, runnable construction — creator and joiner built straight from
direct arguments — is in
[`tests/standalone_construction.rs`](tests/standalone_construction.rs).

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
