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
    core::{
        ConsensusPlugin, ConsensusServiceFor, ConversationConfig, ConversationEvent,
        ConversationPluginsFactory, ConversationQueues, ConversationStateMachine,
        PeerScoringPlugin, ScoringConfig, StewardListConfig, StewardListPlugin,
    },
    member_id::MemberId,
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::MemberWelcome,
    session::{Conversation, ConversationError, ConversationState, PhaseTimer},
};

/// Everything one conversation needs to come into being.
///
/// The plug-in factory is borrowed (one serves every conversation an
/// integrator runs) and the identity is borrowed too — the constructor
/// snapshots its bytes/display. The consensus service is owned: each
/// conversation gets its own, and how services share storage is the
/// integrator's wiring (see the gateway's `ConsensusContext`).
pub struct ConversationDeps<'a, P: ConsensusPlugin, CP: ConversationPluginsFactory, Sig: Signer> {
    /// Builds the per-conversation MLS / scoring / steward plug-ins.
    pub plugins: &'a CP,
    /// This conversation's consensus service, ready to use. The conversation
    /// subscribes to its event bus at construction.
    pub consensus: ConsensusServiceFor<P>,
    /// The local member's MLS signer. Stored on the conversation and passed
    /// into every signing call; the MLS service holds no identity material.
    pub signer: Sig,
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

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory, Sig: Signer> Conversation<P, CP, Sig> {
    /// Create a brand-new conversation we steward. Starts in `Working` with
    /// the local member installed as sole steward at epoch 0. The creator's
    /// own `key_package` supplies the leaf credential and ciphersuite.
    pub fn create(
        conversation_id: &str,
        key_package: &[u8],
        deps: ConversationDeps<P, CP, Sig>,
    ) -> Result<Self, ConversationError> {
        Self::build(conversation_id, deps, Some(key_package))
    }

    /// Join an existing conversation. Starts in `PendingJoin` with no MLS
    /// state; the steward list and scoring fill in once the welcome and
    /// `ConversationSync` arrive.
    pub fn join(
        conversation_id: &str,
        deps: ConversationDeps<P, CP, Sig>,
    ) -> Result<Self, ConversationError> {
        Self::build(conversation_id, deps, None)
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
        deps: ConversationDeps<P, CP, Sig>,
        welcome: &MemberWelcome,
    ) -> Result<Option<Self>, ConversationError> {
        let Some(mls) = deps.plugins.welcome_mls(&welcome.welcome_bytes)? else {
            return Ok(None);
        };
        let conversation_id = mls.conversation_id().to_string();
        let mut conversation = Self::join(&conversation_id, deps)?;
        conversation.complete_join(mls)?;
        conversation.apply_welcome_sync(&welcome.conversation_sync_bytes)?;
        Ok(Some(conversation))
    }

    /// Shared construction body for [`Self::create`] / [`Self::join`].
    /// `key_package` is `Some` on the creator path (its leaf seeds the group)
    /// and `None` on the joiner path.
    fn build(
        conversation_id: &str,
        deps: ConversationDeps<P, CP, Sig>,
        key_package: Option<&[u8]>,
    ) -> Result<Self, ConversationError> {
        let self_member_id_bytes = deps.identity.member_id_bytes().to_vec();
        let is_creation = key_package.is_some();

        let (queues, mls_opt, state_machine, phase_timer) = if let Some(kp) = key_package {
            let mls = deps
                .plugins
                .create_mls(conversation_id.to_string(), kp, &deps.signer)?;
            (
                ConversationQueues::new(conversation_id),
                Some(mls),
                ConversationStateMachine::new_as_member(),
                PhaseTimer::new(),
            )
        } else {
            // Anchor the timer at "now" so `is_pending_join_expired` can
            // detect the 3× commit-inactivity timeout.
            let mut phase_timer = PhaseTimer::new();
            phase_timer.start();
            (
                ConversationQueues::new(conversation_id),
                None,
                ConversationStateMachine::new_as_pending_join(),
                phase_timer,
            )
        };

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
            deps.signer,
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
