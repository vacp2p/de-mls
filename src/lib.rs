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
//! Every driving call — `handle_control`, `admit_candidate` +
//! `handle_candidate`, the report calls (`own_candidate_built`,
//! `commit_applied`, `decision_failed`, `key_package_checked`), the local
//! actions, and `tick` — returns an [`Output`]: decisions to execute
//! against the group, bytes to seal and send, events to observe, and the
//! next wakeup. Executing a decision leads to a report call that returns
//! its own `Output`, so the router loops until one is empty.
//!
//! ```rust,ignore
//! struct Router {
//!     mls: MlsService,             // application-owned group
//!     engine: Engine<KvStore>,
//! }
//!
//! impl Router {
//!     fn on_frame(&mut self, now: Timestamp, frame: Frame) {
//!         let out = match frame {
//!             Frame::Sealed(bytes) => {
//!                 let opened = self.mls.open(&bytes);
//!                 self.engine.handle_control(now, opened.sender, opened.epoch, &opened.plaintext)
//!             }
//!             Frame::Candidate(bytes) => {
//!                 let Admission::Stage { hash, commit } = self.engine.admit_candidate(now, &bytes)? else { return };
//!                 let facts = self.mls.stage(&commit);     // authenticates the sender
//!                 self.engine.handle_candidate(now, hash, facts)
//!             }
//!         };
//!         self.drive(now, out)
//!     }
//!
//!     // Seals and sends `outbound`, executes `decisions` in order (a report
//!     // call's Output feeds back in), then hands `events` to the
//!     // application. Loops until nothing is left; arms `wakeup` for `tick`.
//!     fn drive(&mut self, now: Timestamp, out: Output) { /* .. */ }
//! }
//! ```
//!
//! `tests/engine_bed_flow.rs` and `tests/engine_bed_smoke.rs` drive a fake
//! router and a fake group over this exact loop and stay honest where this
//! sketch cannot.

/// The [`Engine`](engine::Engine) handle: facts in, [`Output`](engine::Output)
/// out, no MLS inside.
pub mod engine;

/// Peer scoring: vocabulary and the library-owned [`PeerScoringService`](peer_scoring::PeerScoringService).
pub mod peer_scoring;

/// Steward-list plug-in: deterministic list and rotation queries.
pub mod steward_list;

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
