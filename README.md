# De-MLS

[![Crates.io](https://img.shields.io/crates/v/de-mls.svg)](https://crates.io/crates/de-mls)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Decentralized MLS proof-of-concept that coordinates secure conversation
membership through off-chain consensus and a Waku relay. A native
desktop client built with Dioxus drives the MLS core directly.

## What's Included

| Crate / Path                  | Description                                                                          |
| ----------------------------- | ------------------------------------------------------------------------------------ |
| **de-mls** (`src/`)           | Core library — MLS conversations, consensus, identity trait, and the delivery layer  |
| **crates/de_mls_ui_protocol** | Shared UI ↔ gateway message types (`AppCmd`, `AppEvent`, `MemberInfo`) + hex display |
| **crates/de_mls_gateway**     | Bridges UI commands to the core runtime and streams events back                      |
| **crates/ui_bridge**          | Bootstrap glue that hosts the async command loop for desktop clients                 |
| **apps/de_mls_desktop_ui**    | Dioxus desktop UI with login, chat, stewardship, and voting flows                    |
| **tests/**                    | Integration tests for the MLS state machine and consensus paths                      |

## Prerequisites

- **Rust** (stable toolchain)
- **Nim** (for building libwaku) — `brew install nim`

## Building libwaku

The project uses a local `libwaku.dylib` built from [logos-messaging-nim](https://github.com/logos-messaging/logos-messaging-nim):

```bash
make          # clones, builds, and copies libwaku.dylib into ./libs/
```

`make` builds `libwaku` with `--undef:metrics` by default to avoid libwaku
metrics thread-label errors in embedded-node usage. Override if needed:

```bash
make LIBWAKU_NIM_PARAMS=""
```

`build.rs` links against `libs/libwaku.dylib` (or `libwaku.so` on Linux) and embeds an rpath automatically.

## Feature Flags

| Feature | Default | Description                                                                     |
| ------- | ------- | ------------------------------------------------------------------------------- |
| `waku`  | off     | Enables the Waku relay transport (`WakuDeliveryService`) and links to `libwaku` |

The core library (`de_mls`) compiles and tests **without** `libwaku` present.
The `waku` feature is required only when you need the concrete `WakuDeliveryService`
implementation — the gateway and desktop crates enable it automatically.

```toml
# Use the transport-agnostic types only (no libwaku needed):
de-mls = { path = "..." }

# Enable the Waku transport (requires libwaku):
de-mls = { path = "...", features = ["waku"] }
```

## Delivery Service (`src/ds/`)

The delivery service (DS) is the transport layer that sits between the MLS core
and the network. It is fully **synchronous** — no tokio dependency — so it can
be used from any Rust context.

### Module layout

``` text
src/ds/
├── mod.rs              Public API re-exports
├── transport.rs        DeliveryService trait, OutboundPacket, InboundPacket
├── error.rs            DeliveryServiceError
├── topic_filter.rs     TopicFilter — HashSet-based allowlist for inbound routing
└── waku/               Waku relay implementation (libwaku FFI)
    ├── mod.rs          WakuDeliveryService, WakuConfig, content-topic helpers
    ├── sys.rs          Raw C FFI bindings (trampoline pattern)
    └── wrapper.rs      Safe WakuNodeCtx wrapper (Drop calls waku_stop)
```

### Key types

| Type                  | Feature | Description                                                                          |
| --------------------- | ------- | ------------------------------------------------------------------------------------ |
| `DeliveryService`     | —       | Trait — `publish()` and `subscribe()`, both synchronous                              |
| `OutboundPacket`      | —       | Payload + conversation id + subtopic + app id (self-message filter)                  |
| `InboundPacket`       | —       | Payload + conversation id + subtopic + app id + timestamp                            |
| `TopicFilter`         | —       | Allowlist used by the gateway to filter inbound packets by conversation              |
| `WakuDeliveryService` | `waku`  | Concrete impl — runs an embedded Waku node on a background `std::thread`             |
| `WakuConfig`          | `waku`  | Node port, discv5 settings                                                           |
| `WakuStartResult`     | `waku`  | Returned by `start()` — contains the service + optional local ENR                    |

### Basic usage

```rust
use de_mls::ds::{WakuDeliveryService, WakuConfig, DeliveryService, OutboundPacket};

let result = WakuDeliveryService::start(WakuConfig {
    node_port: 60000,
    discv5: true,
    discv5_udp_port: 61000,
    ..Default::default()
})?;

let mut ds = result.service;

// Open a pull-style inbound channel (multiple receivers allowed).
let rx = ds.inbound_receiver();
std::thread::spawn(move || {
    while let Ok(pkt) = rx.recv() {
        println!("{} bytes from {}", pkt.payload.len(), pkt.conversation_id);
    }
});

// Publish a message.
ds.publish(OutboundPacket::new(
    b"hello".to_vec(), "app_msg", "my-conversation", b"instance-id",
))?;

// Shut down explicitly, or just drop all clones.
ds.shutdown();
```

### Threading model

`WakuDeliveryService::start()` spawns a `"waku-node"` thread that owns the
libwaku context. Outbound messages are queued via `std::sync::mpsc`; inbound
events are fanned out to all subscribers. The background thread is wrapped in
`catch_unwind` so a panic in the FFI layer won't crash the process.

### Content topics

Messages are routed by Waku content topics with the format:

``` bash
/{conversation_name}/{version}/{subtopic}/proto
```

Two subtopics are used: `app_msg` (application messages) and `welcome`
(MLS Welcome messages for conversation joins). The pubsub topic is
fixed to `/waku/2/rs/15/1` (cluster 15, shard 1).

### Self-message filtering

Each `User` generates a random UUID (`app_id`) stored in the Waku message
`meta` field. On receive, the application drops packets whose `app_id`
matches the local user's. Waku relay (gossipsub) does not filter
self-messages natively.

## Quick Start

### Environment Variables

| Variable                | Required | Description                                      |
| ----------------------- | -------- | ------------------------------------------------ |
| `NODE_PORT`             | Yes      | TCP port for the embedded Waku node              |
| `DISCV5_BOOTSTRAP_ENRS` | No       | Comma-separated ENR strings for discv5 bootstrap |

discv5 peer discovery is always enabled. The discv5 UDP port is derived
automatically as `NODE_PORT + 1000`. Use a unique `NODE_PORT` per local
client so the embedded Waku nodes do not collide.

### Running Multiple Nodes For Example

Nodes on the same `clusterId`/`shard` discover each other automatically via discv5.
No external relay required — just run multiple local nodes.

**Node 1** (bootstrap node):

```bash
NODE_PORT=60001 cargo run -p de-mls-desktop-ui
```

Copy the `Local ENR: enr:-QE...` line from the logs.

**Nodes 2–4** (bootstrap off node 1):

```bash
NODE_PORT=60002 DISCV5_BOOTSTRAP_ENRS="enr:-QE..." cargo run -p de-mls-desktop-ui
NODE_PORT=60003 DISCV5_BOOTSTRAP_ENRS="enr:-QE..." cargo run -p de-mls-desktop-ui
NODE_PORT=60004 DISCV5_BOOTSTRAP_ENRS="enr:-QE..." cargo run -p de-mls-desktop-ui
```

All nodes discover each other via the DHT and form a gossipsub relay mesh automatically.

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

## Library Usage

The library is identity-agnostic — integrators implement
`de_mls::identity::Identity` (bytes + display) for their own scheme
(wallet, Ed25519 pubkey, account id, …) and construct a `User` directly:

```rust,ignore
use de_mls::app::User;

let user = User::new_with_plugins(
    Box::new(my_identity),   // anything implementing `Identity`
    plugins,                 // a `UserPlugins<P, CP>` bundle
    transport,               // your `DeliveryService` impl
);
```

Reference plug-in implementations (in-memory MLS storage, default
consensus backend over `hashgraph-like-consensus`, in-memory peer-score
storage, etc.) live in `de_mls::defaults` and can be adopted wholesale
or swapped piece-by-piece.

## Development Tips

- `cargo test -p de-mls` – runs core tests (no libwaku required)
- `cargo test -p de-mls --features waku` – includes Waku transport tests (needs libwaku in `libs/`)
- `cargo fmt --all --check` / `cargo clippy --all-targets -- -D warnings` – CI enforces both
- `RUST_BACKTRACE=full` – helpful when debugging state-machine transitions during development

Logs for the desktop UI live in `apps/de_mls_desktop_ui/logs/`; core logs are emitted to stdout as well.

## Contributing

Issues and pull requests are welcome. Please include reproduction steps, relevant logs,
and test coverage where possible.
