//! Standalone construction of a [`Conversation`] — no `User` required.
//!
//! [`Conversation::create`] and [`Conversation::join`] take the OpenMLS
//! provider, the plug-in instances, a ready consensus service, and the durable
//! config as direct arguments. The library builds the per-conversation MLS
//! service ([`OpenMlsService`]) internally — the creator seeds a fresh group,
//! the joiner opens one from the welcome — and returns a conversation ready to
//! drop into a registry.

use std::error::Error as StdError;
use std::sync::Arc;

use openmls::credentials::CredentialWithKey;
use openmls::prelude::Ciphersuite;
use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};

use hashgraph_like_consensus::events::ConsensusEventBus;

use crate::{
    ConsensusPlugin, ConsensusServiceFor, Conversation, ConversationConfig, ConversationError,
    ConversationEvent, ConversationQueues, ConversationServices, ConversationStateMachine,
    PeerScoringPlugin, StewardListPlugin,
    mls_crypto::{MlsService, OpenMlsService},
};

impl<C, Sc, St> Conversation<C, Sc, St>
where
    C: ConsensusPlugin,
    Sc: PeerScoringPlugin,
    St: StewardListPlugin,
{
    /// Create a brand-new conversation we steward. Starts in `Working` with the
    /// local member installed as sole steward at epoch 0. The library seeds a
    /// fresh MLS group into `provider` (which it does not retain) from
    /// `credential` and `ciphersuite`. `member_id` names the local member — the
    /// opaque id bytes the protocol matches on.
    #[allow(clippy::too_many_arguments)]
    pub fn create<Pr>(
        conversation_id: &str,
        provider: &Pr,
        credential: CredentialWithKey,
        ciphersuite: Ciphersuite,
        signer: &impl Signer,
        scoring: Sc,
        steward: St,
        consensus: ConsensusServiceFor<C>,
        app_id: Arc<[u8]>,
        config: ConversationConfig,
        member_id: &[u8],
    ) -> Result<Self, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let mls = OpenMlsService::new_as_creator(
            conversation_id.to_string(),
            provider,
            credential,
            ciphersuite,
            signer,
        )?;
        Self::assemble(
            conversation_id,
            mls,
            scoring,
            steward,
            consensus,
            app_id,
            config,
            true,
            member_id,
        )
    }

    /// Build a fully-joined conversation from a welcome: the library opens the
    /// joiner-side MLS group from `welcome_bytes` using `provider` (which must
    /// hold the joiner's key-package private keys), runs the joiner-side join
    /// side-effects, and replays the bundled `ConversationSync` (steward list,
    /// timing, peer scores).
    ///
    /// Returns `Ok(None)` when the welcome doesn't address one of our key
    /// packages — the "not for us" branch, not an error.
    ///
    /// The conversation id comes from the MLS group, so the caller needs no
    /// prior knowledge of the conversation.
    #[allow(clippy::too_many_arguments)]
    pub fn join<Pr>(
        provider: &Pr,
        welcome_bytes: &[u8],
        conversation_sync_bytes: &[u8],
        scoring: Sc,
        steward: St,
        consensus: ConsensusServiceFor<C>,
        app_id: Arc<[u8]>,
        config: ConversationConfig,
        member_id: &[u8],
        signer: &impl Signer,
    ) -> Result<Option<Self>, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let Some(mls) = OpenMlsService::new_from_welcome(provider, welcome_bytes)? else {
            return Ok(None);
        };
        let conversation_id = mls.conversation_id().to_string();
        let mut conversation = Self::assemble(
            &conversation_id,
            mls,
            scoring,
            steward,
            consensus,
            app_id,
            config,
            false,
            member_id,
        )?;
        conversation.on_joined(provider, signer)?;
        conversation.apply_welcome_sync(provider, conversation_sync_bytes, signer)?;
        Ok(Some(conversation))
    }

    /// Shared assembly tail for [`Self::create`] / [`Self::join`]: builds the
    /// queues and consensus subscription around the pre-built MLS service and
    /// plug-in instances. The conversation opens in `Working` and emits the
    /// opening `PhaseChange(Working)` for both paths. `is_creation` bootstraps
    /// the steward list and scoring with the local member (creator) versus
    /// leaving them empty until `ConversationSync` (joiner).
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        conversation_id: &str,
        mls: OpenMlsService,
        mut scoring: Sc,
        mut steward_list: St,
        consensus: ConsensusServiceFor<C>,
        app_id: Arc<[u8]>,
        config: ConversationConfig,
        is_creation: bool,
        member_id: &[u8],
    ) -> Result<Self, ConversationError> {
        let self_member_id_bytes = member_id.to_vec();
        let queues = ConversationQueues::new(conversation_id);

        steward_list.set_max_retries(config.max_reelection_attempts);
        // Creator path: bootstrap the list with self as sole steward at
        // epoch 0. Joiner path leaves the plug-in empty until `ConversationSync`.
        if is_creation {
            steward_list.install_list(0, std::slice::from_ref(&self_member_id_bytes), 1, 0)?;
            // Creator is self at `default_score`; under standard config
            // (`default > threshold`) no cross fires, so we drop the result.
            let _ = scoring.add_member(&self_member_id_bytes);
        }

        let state_machine = ConversationStateMachine::new_as_member();
        let initial_state = state_machine.current_state();

        let consensus_rx = consensus.event_bus().subscribe();
        let services = ConversationServices {
            mls,
            scoring,
            steward_list,
            consensus,
            consensus_rx,
        };
        let conversation = Conversation::new(
            conversation_id.to_string(),
            queues,
            services,
            state_machine,
            config,
            Arc::from(member_id),
            app_id,
        );
        // Surface the opening phase so a caller draining conversation events sees
        // the conversation's starting state without a separate query.
        conversation.emit_event(ConversationEvent::PhaseChange(initial_state));
        Ok(conversation)
    }
}
