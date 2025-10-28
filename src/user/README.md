# User Module

The `user` module encapsulates everything the desktop gateway needs to manage a single MLS participant.  
It owns the MLS identity, tracks the local view of every group the user joined,
drives steward epochs, and bridges consensus messages between the core runtime, Waku network, and UI.

## Directory Layout

``` bash
src/user/
├── consensus.rs   # Voting lifecycle, consensus event fan-in/out
├── groups.rs      # Group creation/join/leave utilities and state queries
├── messaging.rs   # Application-level messaging helpers (ban requests, signing)
├── mod.rs         # `User` actor definition and shared types
├── proposals.rs   # Steward batch proposal processing and pending queues
├── README.md      # This file
├── steward.rs     # Steward-only helpers: epochs, proposals, apply flow
└── waku.rs        # Waku ingestion + routing to per-topic handlers
```

Each file extends `impl User` with domain-specific responsibilities,
keeping the top-level actor (`mod.rs`) lean.

## Core Concepts

- **`User` actor** – Holds one `Identity`, an `MlsProvider`,
  a map of per-group `Arc<RwLock<Group>>`, the consensus facade, and the Ethereum signer.

- **`UserAction` enum (`mod.rs`)** – Return type for most handlers.  
  Signals what the caller (gateway) should do next:
  - `SendToWaku(WakuMessageToSend)`
  - `SendToApp(AppMessage)`
  - `LeaveGroup(String)` (triggers UI + cleanup)
  - `DoNothing` (no side effects)

- **Per-group concurrency** – Keeps a `HashMap<String, Arc<RwLock<Group>>>`.  
  Each group has its own `RwLock`, allowing independent state transitions without blocking unrelated groups.

- **Pending batches (`proposals.rs`)** – Non-steward clients may receive `BatchProposalsMessage` while
  still catching up to state `Waiting`.  
  `PendingBatches` stores the payload until the group is ready to apply it.

## Lifecycle Overview

1. **Login** – The gateway calls `User::new` with a private key.  
   The actor derives an MLS identity (`mls_crypto::Identity`)
   and keeps an `alloy::PrivateKeySigner` for consensus signatures.

2. **Group discovery** – `groups.rs` exposes helpers:
   - `create_group` – Either launches a steward-owned MLS group or prepares a placeholder for later joining.
   - `join_group` – Applies a received `Welcome` message to hydrate the MLS state.
   - `get_group_members` / `get_group_state` / `leave_group` – Used by the UI for metadata and teardown.

3. **Message routing** – Incoming Waku traffic is delivered to `waku.rs::process_waku_message`, which:
   - Filters out self-originated packets (matching `msg.meta` to the group `app_id`).
   - Routes `WELCOME_SUBTOPIC` payloads to `process_welcome_subtopic`.
   - Routes `APP_MSG_SUBTOPIC` payloads to `process_app_subtopic`.
   - Converts MLS protocol messages into `GroupAction`s,
   mapping them back into `UserAction`s so the gateway can forward data to the UI or back onto Waku.

4. **Consensus bridge** – `consensus.rs` is the glue to `ConsensusService`:
   - `process_consensus_proposal` and `process_consensus_vote` persist incoming messages and
    transition group state.
   - `process_user_vote` produces either consensus votes (steward) or user votes (regular members)
     and wraps them for Waku transmission.
   - `handle_consensus_event` reacts to events emitted by the service,
   ensuring the MLS state machine aligns with voting outcomes.
   It reuses `handle_consensus_result` to collapse steward/non-steward flows
   and trigger follow-up actions (apply proposals, queue batch data, or leave the group).

5. **Steward duties** – `steward.rs` layers steward-only APIs:
   - Introspection helpers (`is_user_steward_for_group`, `get_current_epoch_proposals`, etc.).
   - Epoch management (`start_steward_epoch`, `get_proposals_for_steward_voting`),
   which kicks the MLS state machine into `Waiting` / `Voting`.
   - Proposal actions (`add_remove_proposal`, `apply_proposals`) that serialize MLS proposals,
   commits, and optional welcomes into `WakuMessageToSend` objects for broadcast.

6. **Application-facing messaging** – `messaging.rs` contains:
   - `build_group_message` – Wraps an `AppMessage` in MLS encryption for the target group.
   - `process_ban_request` – Normalizes addresses, routes steward vs. member behavior
   (queueing steward removal proposals or forwarding the request back to the group).
   - An implementation of `LocalSigner` for `PrivateKeySigner`,
   allowing consensus code to request signatures uniformly.

7. **Proposal batches** – `proposals.rs` handles the post-consensus MLS churn:
   - `process_batch_proposals_message` – Applies proposals, deserializes MLS commits,
    and emits the resulting `UserAction`.
   - `process_pending_batch_proposals` – Replays stored batches once the group transitions into `Waiting`.

## Waku Topics & Message Types

| Subtopic            | Handler                       | Purpose |
|---------------------|-------------------------------|---------|
| `WELCOME_SUBTOPIC`  | `process_welcome_subtopic`    | Steward announcements, encrypted key packages, welcome messages |
| `APP_MSG_SUBTOPIC`  | `process_app_subtopic`        | Batch proposals, encrypted MLS traffic, consensus proposals/votes |

`process_welcome_subtopic` contains the joining handshake logic:

- **GroupAnnouncement** → Non-stewards encrypt and send their key package back.
- **UserKeyPackage** → Stewards decrypt, store invite proposals, and notify the UI.
- **InvitationToJoin** → Non-stewards validate, call `join_group`, and broadcast a system chat message.

## State Machine Touch Points

Although the primary MLS state machine lives in `crate::state_machine`, the user module coordinates transitions by calling:

- `start_steward_epoch_with_validation`  
- `start_voting`, `complete_voting`, `handle_yes_vote`, `handle_no_vote`
- `start_waiting`, `start_consensus_reached`, `start_working`

This ensures both steward and non-steward clients converge on the same `GroupState` after each consensus result or batch commit.

## Extending the Module

When adding new functionality:

1. Decide whether the behavior is steward-specific, consensus-related,
or generic per-group logic, then extend the appropriate file.
Keeping concerns separated avoids monolithic impl blocks.
2. Return a `UserAction` so the caller (gateway) can decide where to forward the outcome.
3. Prefer reusing `group_ref(group_name)` to fetch the per-group lock; release the lock (`drop`) before performing long-running work to avoid deadlocks.
4. If new Waku payloads are introduced, update `process_waku_message` with a deterministic routing branch and ensure the UI/gateway understands the resulting `AppMessage`.

## Related Tests

Integration scenarios that exercise this module live under:

- `tests/user_test.rs`
- `tests/consensus_realtime_test.rs`
- `tests/state_machine_test.rs`
- `tests/consensus_multi_group_test.rs`

These are good references when updating state transitions or consensus flows.
