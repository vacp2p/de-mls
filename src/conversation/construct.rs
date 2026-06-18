//! Standalone construction of a [`Conversation`] from a [`ConversationDeps`]
//! bundle — no `User` required.
//!
//! [`ConversationDeps`] gathers everything a single conversation needs:
//! the shared plug-in factory (borrowed — one serves every conversation an
//! integrator runs), a ready consensus service, the identity, and the
//! per-conversation configs. [`Conversation::create`] and
//! [`Conversation::join`] consume one bundle and return a conversation ready to
//! drop into a registry.

use openmls_traits::signatures::Signer;
use std::sync::Arc;

use hashgraph_like_consensus::events::ConsensusEventBus;
use tracing::info;

use crate::{
    ConsensusPlugin, ConsensusServiceFor, Conversation, ConversationConfig, ConversationError,
    ConversationEvent, ConversationPluginsFactory, ConversationQueues, ConversationState,
    ConversationStateMachine, PeerScoringPlugin, PhaseTimer, ScoringConfig, StewardListConfig,
    StewardListPlugin, member_id::MemberId, mls_crypto::MlsService,
    protos::de_mls::messages::v1::MemberWelcome,
};

/// Everything one conversation needs to come into being.
///
/// The plug-in factory is borrowed (one serves every conversation an
/// integrator runs) and the identity is borrowed too — the constructor
/// snapshots its bytes/display. The consensus service is owned: each
/// conversation gets its own, and how services share storage is the
/// integrator's wiring (see the gateway's `ConsensusContext`).
pub struct ConversationDeps<'a, P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Builds the per-conversation MLS / scoring / steward plug-ins.
    pub plugins: &'a CP,
    /// This conversation's consensus service, ready to use. The conversation
    /// subscribes to its event bus at construction.
    pub consensus: ConsensusServiceFor<P>,
    /// Local participant identity; the constructor snapshots its bytes
    /// and display form onto the conversation.
    pub identity: &'a dyn MemberId,
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
    /// `signer` is the local member's MLS signer, used to seed the group.
    pub fn create(
        conversation_id: &str,
        key_package: &[u8],
        deps: ConversationDeps<P, CP>,
        signer: &impl Signer,
    ) -> Result<Self, ConversationError> {
        let mls = deps
            .plugins
            .create_mls(conversation_id.to_string(), key_package, signer)?;
        Self::assemble(
            conversation_id,
            deps,
            Some(mls),
            ConversationStateMachine::new_as_member(),
            PhaseTimer::new(),
            true,
        )
    }

    /// Join an existing conversation. Starts in `PendingJoin` with no MLS
    /// state; the steward list and scoring fill in once the welcome and
    /// `ConversationSync` arrive.
    pub fn join(
        conversation_id: &str,
        deps: ConversationDeps<P, CP>,
    ) -> Result<Self, ConversationError> {
        // Anchor the timer at "now" so `is_pending_join_expired` can detect the
        // 3× commit-inactivity timeout.
        let mut phase_timer = PhaseTimer::new();
        phase_timer.start();
        Self::assemble(
            conversation_id,
            deps,
            None,
            ConversationStateMachine::new_as_pending_join(),
            phase_timer,
            false,
        )
    }

    /// Build a fully-joined conversation straight from a received
    /// [`MemberWelcome`] — the whole joiner path in one call: attach MLS
    /// from the welcome, complete the join, and replay the bundled
    /// `ConversationSync` (steward list, timing, peer scores).
    ///
    /// Returns `Ok(None)` when the welcome doesn't address this member's
    /// key package — ignore it. The conversation id comes from the welcome
    /// itself, so the caller needs no prior knowledge of the conversation.
    pub fn from_welcome(
        deps: ConversationDeps<P, CP>,
        welcome: &MemberWelcome,
        signer: &impl Signer,
    ) -> Result<Option<Self>, ConversationError> {
        let Some(mls) = deps.plugins.welcome_mls(&welcome.welcome_bytes)? else {
            return Ok(None);
        };
        let conversation_id = mls.conversation_id().to_string();
        let mut conversation = Self::join(&conversation_id, deps)?;
        conversation.complete_join(mls, signer)?;
        conversation.apply_welcome_sync(&welcome.conversation_sync_bytes, signer)?;
        Ok(Some(conversation))
    }

    /// Shared assembly tail for [`Self::create`] / [`Self::join`]: builds the
    /// queues, plug-ins, and consensus subscription around an already-decided
    /// MLS service and opening state. `is_creation` bootstraps the steward list
    /// and scoring with the local member (creator) versus leaving them empty
    /// until `ConversationSync` (joiner).
    fn assemble(
        conversation_id: &str,
        deps: ConversationDeps<P, CP>,
        mls_opt: Option<CP::Mls>,
        state_machine: ConversationStateMachine,
        phase_timer: PhaseTimer,
        is_creation: bool,
    ) -> Result<Self, ConversationError> {
        let self_member_id_bytes = deps.identity.member_id_bytes().to_vec();
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
        // Joiners get tracked at `JoinedConversation` time, once members are known.
        if is_creation {
            // Creator is self at `default_score`; under standard config
            // (`default > threshold`) no cross fires, so we drop the result.
            let _ = scoring.add_member(&self_member_id_bytes);
        }

        let initial_state = state_machine.current_state();
        if initial_state == ConversationState::PendingJoin {
            info!(
                conversation = conversation_id,
                timeout_s = deps.config.commit_inactivity_duration.as_secs() * 3,
                "pending join, awaiting welcome"
            );
        }

        let consensus = deps.consensus;
        let consensus_rx = consensus.event_bus().subscribe();
        let conversation = Conversation::new(
            conversation_id.to_string(),
            queues,
            mls_opt,
            state_machine,
            phase_timer,
            deps.config,
            scoring,
            steward_list,
            consensus,
            consensus_rx,
            Arc::from(deps.identity.member_id_bytes()),
            Arc::from(deps.identity.member_id_display()),
            deps.app_id,
        );
        // Surface the opening phase so a caller draining conversation events sees
        // the conversation's starting state without a separate query.
        conversation.emit_event(ConversationEvent::PhaseChange(initial_state));
        Ok(conversation)
    }
}
