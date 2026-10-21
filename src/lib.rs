//! # DE-MLS: a decentralized MLS policy engine
//!
//! `de-mls` is [`Engine`], a synchronous, caller-driven policy engine for
//! group membership over MLS: proposal voting, steward election and commit
//! selection, recovery, and peer scoring. It imports no MLS library and
//! never touches ciphertext or the group itself — its boundary is protobuf
//! bytes for anything that travels on the wire, plus plain facts (member
//! ids, epoch, hashes).
//!
//! ## Modules
//!
//! - **[`engine`]** — the [`Engine`] handle and everything it drives:
//!   commit-round buffering and selection, consensus wiring, steward-list
//!   housekeeping, peer scoring hooks, and the [`Output`] vocabulary.
//! - **[`peer_scoring`]** — score events, the library-owned
//!   [`PeerScoringService`], and the RFC scoring deltas.
//! - **[`steward_list`]** — the deterministic steward list and its
//!   per-conversation service.
//! - **[`protos`]** — protobuf message definitions.
//! - **[`error`]** — [`ConversationError`].
//!
//! ## The router
//!
//! A conversation has three parties: an MLS service that owns the group
//! (opens and seals frames, stages and merges commits), the engine (owns
//! every protocol decision), and a router between them that executes what
//! the engine returns. The router is application code — this crate ships
//! no router and no MLS service; `mls-reference/README.md` documents the
//! group-operations contract an MLS service must meet, and gives a
//! reference implementation.
//!
//! Every driving call — `handle_control`, `handle_candidate`, the report
//! calls (`commit_applied`, `build_failed`, `merge_failed`), the local
//! actions, and `tick` — returns an [`Output`]: events to observe and the
//! next wakeup. What to do against the group is asked afterwards:
//! `commit_to_build` names the batch the epoch steward builds,
//! `commit_to_apply` names the commit that merges. Control messages leave
//! during the call, through the [`Publish`] the call received. Acting on an
//! answer leads to a report call that returns its own `Output`, so the
//! router loops until nothing is left. A candidate on the wire is the raw
//! MLS commit bytes: the router stages it and hands the facts the group
//! learned straight to `handle_candidate`, which keeps at most one per
//! round — the epoch steward's.
//!
//! ```rust,ignore
//! struct Router {
//!     mls: MlsService,             // application-owned group
//!     engine: Engine<KvStore>,
//!     publisher: Sender,           // implements `Publish`
//! }
//!
//! impl Router {
//!     fn on_frame(&mut self, now: Timestamp, frame: Frame) {
//!         let out = match frame {
//!             Frame::Sealed(bytes) => {
//!                 let opened = self.mls.open(&bytes);
//!                 self.engine.handle_control(now, opened.sender, opened.epoch, &opened.plaintext, &mut self.publisher)
//!             }
//!             Frame::Commit(bytes) => {
//!                 let facts = self.mls.stage(&bytes);      // authenticates the sender
//!                 self.engine.handle_candidate(now, self.mls.name_of(&bytes), facts, &mut self.publisher)
//!             }
//!         };
//!         self.drive(now, out)
//!     }
//!
//!     // Hands `events` to the application, then asks `commit_to_build`
//!     // (build on the group, broadcast the commit bytes, report it back
//!     // through `handle_candidate`) and `commit_to_apply` (merge, report
//!     // `commit_applied`); their outputs feed back in the same way.
//!     // Loops until nothing is left; arms `wakeup` for `tick`.
//!     fn drive(&mut self, now: Timestamp, out: Output) { /* .. */ }
//! }
//! ```
//!
//! `tests/engine_bed_flow.rs` and `tests/engine_bed_smoke.rs` drive a fake
//! router and a fake group over this exact loop and stay honest where this
//! sketch cannot.

/// The [`Engine`] handle: facts in, [`Output`]
/// out, no MLS inside.
pub mod engine;

/// Peer scoring: vocabulary and the library-owned [`PeerScoringService`].
pub mod peer_scoring;

/// Steward-list plug-in: deterministic list and rotation queries.
pub mod steward_list;

/// The three traits the application implements: [`GroupView`],
/// [`EngineStore`], [`Publish`].
pub mod traits;

/// Library error types.
pub mod error;

/// Protobuf message definitions.
pub mod protos;

// Crate-root re-exports so flat-name imports resolve without the owning
// module path.
pub use engine::*;
pub use error::*;
pub use peer_scoring::*;
pub use steward_list::*;
pub use traits::*;
