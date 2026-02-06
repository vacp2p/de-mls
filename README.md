# DE-MLS

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Decentralized MLS proof-of-concept that coordinates secure group membership through
off-chain consensus and a Waku relay.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Your Application                            │
└───────────────────────────────┬─────────────────────────────────────┘
                                │
        ┌───────────────────────┼───────────────────────┐
        ▼                       ▼                       ▼
┌───────────────┐      ┌───────────────┐      ┌───────────────┐
│     core      │      │   mls_crypto  │      │      ds       │
│  (protocol)   │      │ (encryption)  │      │ (transport)   │
└───────────────┘      └───────────────┘      └───────────────┘
```

### Library Modules

| Module | Description |
|--------|-------------|
| `core` | Protocol implementation: message processing, consensus integration, group operations |
| `mls_crypto` | MLS cryptographic operations: OpenMLS wrapper for encryption/decryption |
| `ds` | Delivery service: transport-agnostic messaging with Waku implementation |
| `app` | Reference application layer: multi-group management, state machine, epoch handling |

### Supporting Crates

| Crate | Description |
|-------|-------------|
| `crates/de_mls_gateway` | Bridges UI commands (`AppCmd`) to the core runtime and streams `AppEvent`s back |
| `crates/ui_bridge` | Bootstrap glue that hosts the async command loop for desktop clients |
| `apps/de_mls_desktop_ui` | Dioxus desktop UI with login, chat, stewardship, and voting flows |

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

## Consensus

DE-MLS uses [hashgraph-like-consensus](https://github.com/vacp2p/hashgraph-like-consensus) for
decentralized membership voting. This provides Byzantine fault-tolerant agreement without
requiring a central coordinator.

### How It Works

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Member A  │     │   Member B  │     │   Member C  │
│  (steward)  │     │             │     │             │
└──────┬──────┘     └──────┬──────┘     └──────┬──────┘
       │                   │                   │
       │  1. Create proposal (add/remove)      │
       │──────────────────────────────────────►│
       │                   │                   │
       │  2. Broadcast proposal to all members │
       │◄──────────────────┼───────────────────│
       │                   │                   │
       │  3. Each member votes YES/NO          │
       │◄──────────────────┼───────────────────│
       │                   │                   │
       │  4. Consensus reached (majority)      │
       │                   │                   │
       │  5. Steward commits approved changes  │
       │──────────────────────────────────────►│
       │                   │                   │
```

### Voting Flow

1. **Proposal**: Steward receives a key package (join) or ban request (remove)
2. **Voting**: Proposal is broadcast; all members vote via signed messages
3. **Consensus**: The consensus service tallies votes and emits `ConsensusReached` or `ConsensusFailed`
4. **Commit**: Steward batches approved proposals into an MLS commit at epoch boundary
5. **Welcome**: New members receive a welcome message with group state

### Configuration

The consensus service is configurable via `DeMlsProvider`. The default uses:
- In-memory storage for proposals and votes
- Broadcast event bus for consensus outcomes
- 15-second voting timeout

## Library Usage

### Core Concepts

**Steward**: The group member responsible for batching approved membership changes into MLS commits.
Currently single-steward mode is implemented (the group creator becomes steward).

**Consensus Voting**: Membership changes (add/remove) go through
[hashgraph-like consensus](https://github.com/vacp2p/hashgraph-like-consensus) voting
before being applied. All members vote; majority approval is required.

**Epoch**: A time period during which the steward collects approved proposals. At epoch end,
the steward creates a batch commit with all approved changes.

### Implementing a Custom Application

To build your own chat application, implement the `GroupEventHandler` trait:

```rust
use async_trait::async_trait;
use de_mls::core::{GroupEventHandler, CoreError};
use de_mls::protos::de_mls::messages::v1::AppMessage;
use ds::transport::OutboundPacket;

struct MyHandler {
    transport: MyTransport,
    ui_sender: mpsc::Sender<UiEvent>,
}

#[async_trait]
impl GroupEventHandler for MyHandler {
    async fn on_outbound(
        &self,
        group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CoreError> {
        // Send MLS-encrypted message via your transport
        self.transport.send(packet).await
            .map_err(|e| CoreError::DeliveryError(e.to_string()))
    }

    async fn on_app_message(
        &self,
        group_name: &str,
        message: AppMessage,
    ) -> Result<(), CoreError> {
        // Dispatch to UI based on message.payload variant
        self.ui_sender.send(UiEvent::Message { group_name, message }).await
            .map_err(|e| CoreError::HandlerError(e.to_string()))
    }

    async fn on_joined_group(&self, group_name: &str) -> Result<(), CoreError> {
        self.ui_sender.send(UiEvent::GroupJoined(group_name.to_string())).await
            .map_err(|e| CoreError::HandlerError(e.to_string()))
    }

    async fn on_leave_group(&self, group_name: &str) -> Result<(), CoreError> {
        self.ui_sender.send(UiEvent::GroupRemoved(group_name.to_string())).await
            .map_err(|e| CoreError::HandlerError(e.to_string()))
    }

    async fn on_error(&self, group_name: &str, operation: &str, error: &str) {
        tracing::error!("Error in {operation} for group {group_name}: {error}");
    }
}
```

Then use the `User` struct for group management:

```rust
use de_mls::core::DefaultProvider;
use de_mls::app::User;

let user: User<DefaultProvider> = User::with_private_key(
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    consensus,
    my_handler,
)?;

// Create a group (as steward)
user.create_group("my-group", true).await?;

// Send a message
user.send_message("my-group", b"Hello, world!").await?;
```

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

## Development

```bash
# Run tests
cargo test

# Check formatting and linting
cargo fmt --all --check
cargo clippy

# Build documentation
cargo doc --open
```

Logs for the desktop UI live in `apps/de_mls_desktop_ui/logs/`; core logs are emitted to stdout as well.

## Contributing

Issues and pull requests are welcome. Please include reproduction steps, relevant logs,
and test coverage where possible.
