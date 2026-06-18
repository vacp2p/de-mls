//! Standalone construction of a [`Conversation`] from a [`ConversationDeps`]
//! bundle — no `User` required.
//!
//! [`ConversationDeps`] gathers everything a single conversation needs:
//! the shared plug-in factory (borrowed — one serves every conversation an
//! integrator runs), a ready consensus service, and the per-conversation
//! configs. [`Conversation::create`] and
//! [`Conversation::from_welcome`] consume one bundle and return a conversation
//! ready to drop into a registry.

use openmls_traits::signatures::Signer;
use std::sync::Arc;

use hashgraph_like_consensus::events::ConsensusEventBus;

use crate::{
    ConsensusPlugin, ConsensusServiceFor, Conversation, ConversationConfig, ConversationError,
    ConversationEvent, ConversationPluginsFactory, ConversationQueues, ConversationStateMachine,
    PeerScoringPlugin, PhaseTimer, ScoringConfig, StewardListConfig, StewardListPlugin,
    mls_crypto::MlsService, protos::de_mls::messages::v1::MemberWelcome,
};

/// Everything one conversation needs to come into being.
///
/// The plug-in factory is borrowed (one serves every conversation an
/// integrator runs). The consensus service is owned: each conversation gets
/// its own, and how services share storage is the integrator's wiring (see
/// the gateway's `ConsensusContext`).
pub struct ConversationDeps<'a, P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Builds the per-conversation MLS / scoring / steward plug-ins.
    pub plugins: &'a CP,
    /// This conversation's consensus service, ready to use. The conversation
    /// subscribes to its event bus at construction.
    pub consensus: ConsensusServiceFor<P>,
    /// Per-instance UUID stamped on every outbound packet for echo dedup.
    pub app_id: Arc<[u8]>,
    /// Durable per-conversation protocol config.
    pub config: ConversationConfig,
    /// Seed config for the peer-scoring plug-in.
    pub scoring_config: ScoringConfig,
    /// Seed config for the steward-list plug-in.
    pub steward_list_config: StewardListConfig,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Conversation<P, CP> {
    /// Create a brand-new conversation we steward. Starts in `Working` with
    /// the local member installed as sole steward at epoch 0. The creator's
    /// own `key_package` supplies the leaf credential and ciphersuite.
    /// `member_id` / `member_id_display` name the local member — the opaque id
    /// bytes the protocol matches on and the human-readable form. `signer` is
    /// the member's MLS signer, used to seed the group.
    pub fn create(
        conversation_id: &str,
        key_package: &[u8],
        deps: ConversationDeps<P, CP>,
        member_id: &[u8],
        member_id_display: &str,
        signer: &impl Signer,
    ) -> Result<Self, ConversationError> {
        let mls = deps
            .plugins
            .create_mls(conversation_id.to_string(), key_package, signer)?;
        Self::assemble(
            conversation_id,
            deps,
            mls,
            true,
            member_id,
            member_id_display,
        )
    }

    /// Build a fully-joined conversation straight from a received
    /// [`MemberWelcome`] — the whole joiner path in one call: seed MLS from
    /// the welcome, run the joiner-side join side-effects, and replay the
    /// bundled `ConversationSync` (steward list, timing, peer scores).
    ///
    /// Returns `Ok(None)` when the welcome doesn't address this member's
    /// key package — ignore it. The conversation id comes from the welcome
    /// itself, so the caller needs no prior knowledge of the conversation.
    pub fn from_welcome(
        deps: ConversationDeps<P, CP>,
        welcome: &MemberWelcome,
        member_id: &[u8],
        member_id_display: &str,
        signer: &impl Signer,
    ) -> Result<Option<Self>, ConversationError> {
        let Some(mls) = deps.plugins.welcome_mls(&welcome.welcome_bytes)? else {
            return Ok(None);
        };
        let conversation_id = mls.conversation_id().to_string();
        let mut conversation = Self::assemble(
            &conversation_id,
            deps,
            mls,
            false,
            member_id,
            member_id_display,
        )?;
        conversation.on_joined(signer)?;
        conversation.apply_welcome_sync(&welcome.conversation_sync_bytes, signer)?;
        Ok(Some(conversation))
    }

    /// Shared assembly tail for [`Self::create`] / [`Self::from_welcome`]:
    /// builds the queues, plug-ins, and consensus subscription around an
    /// already-seeded MLS service. The conversation opens in `Working` and
    /// emits the opening `PhaseChange(Working)` for both paths. `is_creation`
    /// bootstraps the steward list and scoring with the local member (creator)
    /// versus leaving them empty until `ConversationSync` (joiner).
    fn assemble(
        conversation_id: &str,
        deps: ConversationDeps<P, CP>,
        mls: CP::Mls,
        is_creation: bool,
        member_id: &[u8],
        member_id_display: &str,
    ) -> Result<Self, ConversationError> {
        let self_member_id_bytes = member_id.to_vec();
        let queues = ConversationQueues::new(conversation_id);

        let mut steward_list = deps
            .plugins
            .make_steward_list(conversation_id.as_bytes(), deps.steward_list_config);
        steward_list.set_max_retries(deps.config.max_reelection_attempts);
        // Creator path: bootstrap the list with self as sole steward at
        // epoch 0. Joiner path leaves the plug-in empty until `ConversationSync`.
        if is_creation {
            steward_list.install_list(0, std::slice::from_ref(&self_member_id_bytes), 1, 0)?;
        }

        let mut scoring = deps.plugins.make_scoring(&deps.scoring_config);
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
