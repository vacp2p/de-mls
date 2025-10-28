# De-MLS

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Decentralized MLS proof-of-concept that coordinates secure group membership through
off-chain consensus and a Waku relay.  
This repository now ships a native desktop client built with Dioxus that drives the MLS core directly.

## What’s Included

- **de-mls** – core library that manages MLS groups, consensus, and Waku integration
- **crates/de_mls_gateway** – bridges UI commands (`AppCmd`) to the core runtime and streams `AppEvent`s back
- **crates/ui_bridge** – bootstrap glue that hosts the async command loop for desktop clients
- **apps/de_mls_desktop_ui** – Dioxus desktop UI with login, chat, stewardship, and voting flows
- **tests/** – integration tests that exercise the MLS state machine and consensus paths

## Quick Start

### 1. Launch a test Waku relay

Run a lightweight `nwaku` node that your local clients can connect to:

```bash
docker run \
  -p 8645:8645 \
  -p 60000:60000 \
  wakuorg/nwaku:v0.33.1 \
  --cluster-id=15 \
  --rest \
  --relay \
  --rln-relay=false \
  --pubsub-topic=/waku/2/rs/15/1
```

Take note of the node multiaddr printed in the logs (looks like `/ip4/127.0.0.1/tcp/60000/p2p/<peer-id>`).

### 2. Set the runtime environment

The desktop app reads the same environment variables the MLS core uses:

```bash
export NODE_PORT=60001                # UDP/TCP port the embedded Waku client will bind to             
export PEER_ADDRESSES=/ip4/127.0.0.1/tcp/60000/p2p/<peer-id>
export RUST_LOG=info,de_mls_gateway=info                    # optional; controls UI + gateway logging
```

Use a unique `NODE_PORT` per local client so the embedded Waku nodes do not collide.
`PEER_ADDRESSES` accepts a comma-separated list if you want to bootstrap from multiple relays.

### 3. Launch the desktop application

```bash
cargo run -p de_mls_desktop_ui
```

The first run creates `apps/de_mls_desktop_ui/logs/de_mls_ui.log` and starts the event bridge
and embedded Waku client.
Repeat steps 2–3 in another terminal with a different `NODE_PORT` to simulate multiple users.

## Using the Desktop UI

- **Login screen** – paste an Ethereum-compatible secp256k1 private key (hex, with or without `0x`)
  and click `Enter`.  
  On success the app derives your wallet address, stores it in session state,
  and navigates to the home layout.

- **Header bar** – shows the derived address and allows runtime log-level changes (`error`→`trace`).  
  Log files rotate daily under `apps/de_mls_desktop_ui/logs/`.

- **Groups panel** – lists every MLS group returned by the gateway.  
  Use `Create` or `Join` to open a modal, enter the group name,
  and the UI automatically refreshes the list and opens the group.

- **Chat panel** – displays live conversation messages for the active group.  
  Compose text messages at the bottom; the UI also offers:
  - `Leave group` to request a self-ban (the backend fills in your address)
  - `Request ban` to request ban for another user
  Member lists are fetched automatically when a group is opened so you can
  pick existing members from the ban modal.

- **Consensus panel** – keeps stewards and members aligned:
  - Shows whether you are a steward for the active group
  - Lists pending steward requests collected during the current epoch
  - Surfaces the proposal currently open for voting with `YES`/`NO` buttons
  - Stores the latest proposal decisions with timestamps for quick auditing

## Steward State Machine

- **Working** – normal mode; all MLS messages are allowed
- **Waiting** – a steward epoch is active; only the steward may push `BATCH_PROPOSALS_MESSAGE`
- **Voting** – the consensus phase; everyone may submit `VOTE`/`USER_VOTE`,
  the steward can still publish proposal metadata

Transitions:

```text
Working --start_steward_epoch()--> Waiting (if proposals exist)
Working --start_steward_epoch()--> Working (if no proposals)
Waiting --start_voting()---------> Voting
Waiting --no_proposals_found()---> Working
Voting --complete_voting(YES)----> Waiting --apply_proposals()--> Working
Voting --complete_voting(NO)-----> Working
```

Stewards always return to `Working` after an epoch finishes;
edge cases such as missing proposals are handled defensively with detailed tracing.

## Development Tips

- `cargo test` – runs the Rust unit + integration test suite
- `cargo fmt --all check` / `cargo clippy` – keep formatting and linting consistent with the codebase
- `RUST_BACKTRACE=full` – helpful when debugging state-machine transitions during development

Logs for the desktop UI live in `apps/de_mls_desktop_ui/logs/`; core logs are emitted to stdout as well.

## Contributing

Issues and pull requests are welcome. Please include reproduction steps, relevant logs,
and test coverage where possible.
