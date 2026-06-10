//! Standalone construction of a [`SessionRunner`] from a [`ConversationDeps`]
//! bundle — no `User` required.
//!
//! [`ConversationDeps`] gathers everything a single conversation needs:
//! the shared plug-in factory and consensus context (borrowed — one set
//! serves every conversation an integrator runs), the identity, the
//! transport, and the per-conversation config. [`SessionRunner::create`]
//! and [`SessionRunner::join`] consume one bundle and return a runner ready
//! to drop into a registry. `User` is now a thin caller of these; an
//! integrator can build conversations the same way without it.

use std::sync::Arc;

use hashgraph_like_consensus::events::ConsensusEventBus;
use tracing::info;

use crate::{
    core::{
        ConsensusPlugin, ConversationConfig, ConversationPluginsFactory, ConversationQueues,
        ConversationStateMachine, PeerScoringPlugin, ScoringConfig, SessionEvent,
        StewardListConfig, StewardListPlugin,
    },
    member_id::MemberId,
    session::{ConsensusContext, ConversationState, PhaseTimer, SessionRunner, UserError},
};

/// Everything one conversation needs to come into being.
///
/// The plug-in factory and consensus context are borrowed: an integrator
/// holds one of each and serves every conversation from them. The identity
/// is borrowed too — the constructor snapshots its bytes/display. The rest
/// is owned and cheap (Arc handles + small configs), built fresh per
/// conversation since `config` varies.
pub struct ConversationDeps<'a, P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Builds the per-conversation MLS / scoring / steward plug-ins.
    pub plugins: &'a CP,
    /// Mints this conversation's consensus service from shared storage.
    pub consensus: &'a ConsensusContext<P>,
    /// Local participant identity; the constructor snapshots its bytes
    /// and display form onto the runner.
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

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Build a runner for a brand-new conversation we create and steward.
    /// Starts in `Working` with the local member installed as sole steward
    /// at epoch 0.
    pub fn create(conversation_id: &str, deps: ConversationDeps<P, CP>) -> Result<Self, UserError> {
        Self::build(conversation_id, deps, true)
    }

    /// Build a runner that joins an existing conversation. Starts in
    /// `PendingJoin` with no MLS state; the steward list and scoring fill in
    /// once the welcome and `ConversationSync` arrive.
    pub fn join(conversation_id: &str, deps: ConversationDeps<P, CP>) -> Result<Self, UserError> {
        Self::build(conversation_id, deps, false)
    }

    /// Shared construction body for [`Self::create`] / [`Self::join`].
    fn build(
        conversation_id: &str,
        deps: ConversationDeps<P, CP>,
        is_creation: bool,
    ) -> Result<Self, UserError> {
        let self_member_id_bytes = deps.identity.member_id_bytes().to_vec();

        let (queues, mls_opt, state_machine, phase_timer) = if is_creation {
            let mls = deps.plugins.create_mls(conversation_id.to_string())?;
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

        let consensus = deps.consensus.build_service();
        let consensus_rx = consensus.event_bus().subscribe();
        let runner = SessionRunner::new(
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
        // Surface the opening phase so a caller draining session events sees
        // the conversation's starting state without a separate query.
        runner.emit_event(SessionEvent::PhaseChange(initial_state));
        Ok(runner)
    }
}
