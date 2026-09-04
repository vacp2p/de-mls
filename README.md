# De-MLS

[![Crates.io](https://img.shields.io/crates/v/de-mls.svg)](https://crates.io/crates/de-mls)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Decentralized MLS — the policy engine behind end-to-end encrypted group
messaging with consensus-based membership over gossipsub-like networks.
de-mls implements the [Decentralized MLS Off-Chain Consensus](https://github.com/logos-co/logos-lips/blob/master/docs/anoncomms/raw/decentralized-mls-offchain-consensus.md)
protocol: proposal voting, steward election and commit selection, recovery,
and peer scoring, driven by a hashgraph-like consensus service. It imports no
MLS library and never touches ciphertext or the group itself.

> Looking for a runnable app? An example integration — a gateway, a Waku
> delivery service, and a Dioxus desktop client wired onto this library —
> lives in the **[de-mls-poc](https://github.com/vacp2p/de-mls-poc)**
> repository.

## What you own vs. what de-mls owns

**You provide:** the MLS group itself, behind the `mls-reference` crate's
group-operations contract — creation, welcomes, sealing and opening frames,
staging and merging commits — plus the router that drives the engine and
executes what it returns, transport, and key-package minting.

**de-mls owns:** the protocol — proposal voting, steward election, commit
selection, recovery, and peer scoring — along with the per-conversation state
behind it: proposal queues, deduplication, the steward list, and peer scores.

## Three parties, one loop

A conversation has three parties: an MLS service that owns the group, the
[`Engine`](src/engine/mod.rs) (this crate), and a router between them that is
application code. The router opens every frame, hands the engine control
bytes with the MLS-authenticated sender, and executes what the engine
returns — a decision against the group, bytes to seal and send, an event to
observe. Executing a decision leads to a report call that returns its own
output, so the router loops until one is empty; see `src/lib.rs` for the loop
in full.

`mls-reference/README.md` documents the group-operations contract an MLS
service must meet and the router contract each call is held to, and ships a
reference implementation over OpenMLS plus a conformance suite.

## Consensus

Membership changes are agreed by vote before they commit, and de-mls owns
that orchestration end to end: opening a proposal, collecting votes, the
auto-vote and timeout deadlines, and turning a resolved decision into the
next steward commit or election. The engine runs the
`hashgraph-like-consensus` library over in-memory sessions; the conversation
id serves as the consensus scope. A session ends with a verdict: approved,
rejected, or failed when the timeout passes with no decision — a rejection
is final for the epoch, a failure may be filed again.

## Peer scoring

de-mls turns observed protocol events into score deltas and keeps the
per-member table. What a score *means* is the application's policy: a member
whose score moves surfaces as a `MemberScoreChanged { member, previous,
score }` event and keeps every protocol right it had. Compare either end
against your own removal threshold, or watch the trend. Act on it by calling
`propose_remove`, or `propose_score_removal` to raise the emergency the group
votes on — any member may raise one, and on YES it commits immediately
instead of waiting out the inactivity timer.

Scores are per-node: they travel once, in a joiner's `ConversationSync`
bootstrap, and after that each member scores what it observes, so two
members can hold different scores for the same peer.

## Steward list

Who may commit each epoch — the deterministic steward list and its epoch
rotation — is fully engine-owned. The application sets only its size bounds
(`sn_min` / `sn_max`, the `steward_list` field on `EngineConfig`); de-mls
generates the list, validates election proposals, runs the election through
consensus, and rotates the epoch steward. Only the epoch steward's candidate
may win a commit round (RFC deviation, tracked); a silent steward is skipped
by a vote rather than covered by a backup timer.

Generation is deterministic and normative: every member derives the
identical steward list by sorting on `SHA256(epoch ‖ retry_round ‖ member_id
‖ conversation_id)`, and a proposal that doesn't reproduce it is rejected by
all peers (RFC §"Steward list creation").

## Build & test

```bash
cargo build --workspace
cargo test  --workspace --release
cargo clippy --workspace --tests -- -D warnings
RUSTDOCFLAGS='-Dwarnings' cargo doc --workspace --no-deps --document-private-items
```

Building requires the Rust toolchain (edition 2024) and `protoc`, which
`build.rs` uses to compile the protobuf definitions.

## Documentation

- **API:** every public item carries its contract in rustdoc — run
  `cargo doc -p de-mls --open`.
- **The bed:** `tests/engine_bed_flow.rs` and `tests/engine_bed_smoke.rs`
  drive the engine end to end over a fake router and a fake group.
- **Protocol:** de-mls follows the
  [Decentralized MLS Off-Chain Consensus](https://github.com/logos-co/logos-lips/blob/master/docs/anoncomms/raw/decentralized-mls-offchain-consensus.md)
  specification.

## Contributing

Issues and pull requests are welcome. Please include reproduction steps, relevant
logs, and test coverage where possible.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option.
