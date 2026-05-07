// //! Steward commit candidate creation and group member queries.

// use prost::Message;
// use tracing::info;

// use crate::{
//     core::{
//         api::compute_commit_hash,
//         error::CoreError,
//         group::{BufferedCommitCandidate, Group, member_set},
//         proposal_kind::ProposalKind,
//         steward_list_plugin::StewardListPlugin,
//     },
//     ds::{APP_MSG_SUBTOPIC, OutboundPacket},
//     mls_crypto::{
//         CommitCandidate as MlsCommitCandidate, KeyPackageBytes, MlsCommitInput, MlsService,
//     },
//     protos::de_mls::messages::v1::{AppMessage, CommitCandidate, group_update_request},
// };

// // ─────────────────────────── Steward Operations ───────────────────────────

// /// Build a commit candidate and buffer it for [`crate::core::finalize_freeze_round`].
// ///
// /// The gate is plain steward-list membership — intentionally **not**
// /// epoch-steward or list-exhaustion, so members of the *previous* list
// /// can commit recovery actions when an election fails.
// /// `Group::is_in_recovery_mode()` (Layer 3) bypasses the gate entirely.
// pub fn create_commit_candidate<M: MlsService>(
//     group: &mut Group,
//     mls: &M,
//     steward: &dyn StewardListPlugin,
//     self_identity: &[u8],
//     app_id: &[u8],
// ) -> Result<Option<OutboundPacket>, CoreError> {

// }
