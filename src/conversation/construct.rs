//! Standalone construction of a [`Conversation`] from a [`ConversationDeps`]
//! bundle — no `User` required.
//!
//! [`ConversationDeps`] gathers everything a single conversation needs:
//! the pre-built per-conversation plug-in instances, a ready consensus
//! service, and the durable config. [`Conversation::create`] and
//! [`Conversation::join`] consume one bundle and return a conversation
//! ready to drop into a registry.

use openmls_traits::signatures::Signer;
use std::sync::Arc;

use hashgraph_like_consensus::events::ConsensusEventBus;

use crate::{
    ConsensusPlugin, ConsensusServiceFor, Conversation, ConversationConfig, ConversationError,
    ConversationEvent, ConversationPlugins, ConversationQueues, ConversationStateMachine,
    PeerScoringPlugin, PhaseTimer, StewardListPlugin, mls_crypto::MlsService,
};

/// Everything one conversation needs to come into being.
///
/// The plug-in instances are owned: the integrator builds the MLS, scoring,
/// and steward-list plug-ins for this conversation and hands them over. The
/// consensus service is owned too — each conversation gets its own, and how
/// services share storage is the integrator's wiring (see the gateway's
/// `ConsensusContext`).
pub struct ConversationDeps<P: ConsensusPlugin, CP: ConversationPlugins> {
    /// This conversation's MLS service, already seeded (creator) or opened
    /// from a welcome (joiner).
    pub mls: CP::Mls,
    /// Fresh per-conversation peer-scoring plug-in.
    pub scoring: CP::Scoring,
    /// Fresh per-conversation steward-list plug-in.
    pub steward: CP::StewardList,
    /// This conversation's consensus service, ready to use. The conversation
    /// subscribes to its event bus at construction.
    pub consensus: ConsensusServiceFor<P>,
    /// Per-instance UUID stamped on every outbound packet for echo dedup.
    pub app_id: Arc<[u8]>,
    /// Durable per-conversation protocol config.
    pub config: ConversationConfig,
}

impl<P: ConsensusPlugin, CP: ConversationPlugins> Conversation<P, CP> {
    /// Create a brand-new conversation we steward. Starts in `Working` with
    /// the local member installed as sole steward at epoch 0. The MLS service
    /// in `deps` is already seeded by the integrator. `member_id` /
    /// `member_id_display` name the local member — the opaque id bytes the
    /// protocol matches on and the human-readable form.
    pub fn create(
        conversation_id: &str,
        deps: ConversationDeps<P, CP>,
        member_id: &[u8],
        member_id_display: &str,
    ) -> Result<Self, ConversationError> {
        Self::assemble(conversation_id, deps, true, member_id, member_id_display)
    }

    /// Build a fully-joined conversation from a welcome the integrator already
    /// opened: the joiner-side MLS service arrives in `deps.mls`. Runs the
    /// joiner-side join side-effects and replays the bundled `ConversationSync`
    /// (steward list, timing, peer scores).
    ///
    /// The conversation id comes from the MLS service, so the caller needs no
    /// prior knowledge of the conversation.
    pub fn join(
        deps: ConversationDeps<P, CP>,
        conversation_sync_bytes: &[u8],
        member_id: &[u8],
        member_id_display: &str,
        signer: &impl Signer,
    ) -> Result<Self, ConversationError> {
        let conversation_id = deps.mls.conversation_id().to_string();
        let mut conversation =
            Self::assemble(&conversation_id, deps, false, member_id, member_id_display)?;
        conversation.on_joined(signer)?;
        conversation.apply_welcome_sync(conversation_sync_bytes, signer)?;
        Ok(conversation)
    }

    /// Shared assembly tail for [`Self::create`] / [`Self::join`]: builds the
    /// queues and consensus subscription around the pre-built plug-in
    /// instances moved out of `deps`. The conversation opens in `Working` and
    /// emits the opening `PhaseChange(Working)` for both paths. `is_creation`
    /// bootstraps the steward list and scoring with the local member (creator)
    /// versus leaving them empty until `ConversationSync` (joiner).
    fn assemble(
        conversation_id: &str,
        deps: ConversationDeps<P, CP>,
        is_creation: bool,
        member_id: &[u8],
        member_id_display: &str,
    ) -> Result<Self, ConversationError> {
        let self_member_id_bytes = member_id.to_vec();
        let queues = ConversationQueues::new(conversation_id);

        let mls = deps.mls;
        let mut scoring = deps.scoring;
        let mut steward_list = deps.steward;

        steward_list.set_max_retries(deps.config.max_reelection_attempts);
        // Creator path: bootstrap the list with self as sole steward at
        // epoch 0. Joiner path leaves the plug-in empty until `ConversationSync`.
        if is_creation {
            steward_list.install_list(0, std::slice::from_ref(&self_member_id_bytes), 1, 0)?;
        }

        // Joiners get tracked at join time, once members are known.
        if is_creation {
            // Creator is self at `default_score`; under standard config
            // (`default > threshold`) no cross fires, so we drop the result.
            let _ = scoring.add_member(&self_member_id_bytes);
        }

        let state_machine = ConversationStateMachine::new_as_member();
        let initial_state = state_machine.current_state();

        let consensus = deps.consensus;
        let consensus_rx = consensus.event_bus().subscribe();
        let conversation = Conversation::new(
            conversation_id.to_string(),
            queues,
            mls,
            state_machine,
            PhaseTimer::new(),
            deps.config,
            scoring,
            steward_list,
            consensus,
            consensus_rx,
            Arc::from(member_id),
            Arc::from(member_id_display),
            deps.app_id,
        );
        // Surface the opening phase so a caller draining conversation events sees
        // the conversation's starting state without a separate query.
        conversation.emit_event(ConversationEvent::PhaseChange(initial_state));
        Ok(conversation)
    }
}
