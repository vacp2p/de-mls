//! Standalone construction of a [`Conversation`].
//!
//! [`Conversation::create`] and [`Conversation::join`] take the OpenMLS
//! provider, the plug-in instances, the consensus backend, and the durable
//! config as direct arguments. The library builds the per-conversation MLS
//! service ([`MlsService`]) and consensus service internally — the creator
//! seeds a fresh group, the joiner opens one from the welcome — and returns a
//! conversation ready to drop into a registry.

use std::error::Error as StdError;
use std::sync::Arc;

use openmls::credentials::CredentialWithKey;
use openmls::group::MlsGroupCreateConfig;
use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};

use hashgraph_like_consensus::{events::ConsensusEventBus, service::ConsensusService};

use crate::{
    ConsensusPlugin, Conversation, ConversationConfig, ConversationError, ConversationEvent,
    ConversationQueues, ConversationServices, ConversationStateMachine, PeerScoreStorage,
    PeerScoringService, StewardListService, WallClock,
    consensus::outcome_bus::OutcomeBus,
    mls_crypto::{MlsCommitInput, MlsService, member_id_of, signature_key_of_key_package},
    protos::de_mls::messages::v1::MemberWelcome,
};

impl<Cp, Sc, Wc> Conversation<Cp, Sc, Wc>
where
    Cp: ConsensusPlugin,
    Sc: PeerScoreStorage,
    Wc: WallClock,
{
    /// Create a new conversation as the steward.
    /// If you want a single-steward group, do not pass any `initial_members`.
    /// If you want to start with a group of members, provide their key packages
    /// in `initial_members` so all are added in a single genesis commit—group
    /// starts at epoch 1 with all present.
    /// In both cases, the creator is installed as a steward and the group is ready to use.
    #[allow(clippy::too_many_arguments)]
    pub fn create<Pr>(
        conversation_id: &str,
        provider: &Pr,
        credential: CredentialWithKey,
        group_config: &MlsGroupCreateConfig,
        signer: &impl Signer,
        consensus: &Cp,
        scoring: PeerScoringService<Sc>,
        clock: Wc,
        app_id: Arc<[u8]>,
        config: ConversationConfig,
        initial_members: &[&[u8]],
    ) -> Result<Self, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let mls = MlsService::new_as_creator(
            conversation_id.to_string(),
            provider,
            credential,
            group_config,
            signer,
        )?;
        let mut conversation = Self::assemble(
            conversation_id,
            mls,
            scoring,
            consensus,
            clock,
            app_id,
            config,
            true,
        )?;
        conversation.seed_initial_members(provider, signer, initial_members)?;
        Ok(conversation)
    }

    /// Admit all initial members in a single genesis commit,
    /// merge inline (no proposal or vote), install the steward list, and emit a welcome.
    /// Their join epochs are unset, making them eligible stewards from epoch 1.
    fn seed_initial_members<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        initial_members: &[&[u8]],
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if initial_members.is_empty() {
            return Ok(());
        }
        let updates: Vec<MlsCommitInput> = initial_members
            .iter()
            .map(|kp| MlsCommitInput::Add(kp.to_vec()))
            .collect();
        let joiner_identities: Vec<Vec<u8>> = initial_members
            .iter()
            .map(|kp| signature_key_of_key_package(kp))
            .collect::<Result<_, _>>()?;

        // Genesis is the creator's unilateral seed: build the batched add-commit
        // and merge it inline, skipping the consensus/freeze lifecycle.
        let artifacts = self
            .services
            .mls
            .create_commit_candidate(provider, signer, &updates)?;
        let delta = self.services.mls.merge_own_commit(provider)?;

        // Score the members by their assigned leaf and install the steward list
        // over the settled members at the merged epoch.
        for m in &delta.added {
            self.services.scoring.add_member(&member_id_of(m.index))?;
        }
        let (epoch, members) = {
            let mls = self.mls();
            (mls.current_epoch()?, mls.members()?)
        };
        let settled = self.queues.settled_members(&members, epoch);
        let sn = self
            .services
            .steward_list
            .config()
            .compute_list_size(settled.len());
        self.services
            .steward_list
            .install_list(epoch, &settled, sn, 0)?;

        // Deliver the single welcome: attach the ConversationSync, emit
        // WelcomeReady, and broadcast it for relay.
        if let Some(welcome_bytes) = artifacts.welcome {
            self.deliver_welcome(
                provider,
                signer,
                MemberWelcome {
                    welcome_bytes,
                    conversation_sync_bytes: Vec::new(),
                    joiner_identities,
                },
            );
        }
        Ok(())
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
    /// prior knowledge of the conversation. `clock` moves in and becomes
    /// the conversation's time source — every deadline is measured against
    /// it; a shared clock (e.g. [`crate::MockClock`]) stays drivable through
    /// a clone the caller keeps.
    #[allow(clippy::too_many_arguments)]
    pub fn join<Pr>(
        provider: &Pr,
        signer: &impl Signer,
        welcome_bytes: &[u8],
        conversation_sync_bytes: &[u8],
        consensus: &Cp,
        scoring: PeerScoringService<Sc>,
        clock: Wc,
        app_id: Arc<[u8]>,
        config: ConversationConfig,
    ) -> Result<Option<Self>, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let Some(mls) = MlsService::new_from_welcome(provider, welcome_bytes)? else {
            return Ok(None);
        };
        let conversation_id = mls.conversation_id().to_string();
        let mut conversation = Self::assemble(
            &conversation_id,
            mls,
            scoring,
            consensus,
            clock,
            app_id,
            config,
            false,
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
        mls: MlsService,
        mut scoring: PeerScoringService<Sc>,
        consensus: &Cp,
        clock: Wc,
        app_id: Arc<[u8]>,
        config: ConversationConfig,
        is_creation: bool,
    ) -> Result<Self, ConversationError> {
        config.validate()?;
        scoring.validate_config()?;

        // Identity is the member's leaf index: the creator seeds at leaf 0, a
        // joiner takes the leaf the welcome placed it on.
        let self_member_id_bytes = member_id_of(mls.own_index());
        let queues = ConversationQueues::new(conversation_id, config.dedup_window);

        // The conversation id is the deterministic-sort salt every member must
        // share; the library owns it so creator and joiner agree on every
        // elected list.
        let mut steward_list = StewardListService::empty(config.steward_list.clone());
        steward_list.set_conversation_id(conversation_id.as_bytes());
        steward_list.set_max_retries(config.max_reelection_attempts);
        // Creator path: bootstrap the list with self as sole steward at
        // epoch 0. Joiner path leaves the member set empty until `ConversationSync`.
        if is_creation {
            steward_list.install_list(0, std::slice::from_ref(&self_member_id_bytes), 1, 0)?;
            scoring.add_member(&self_member_id_bytes)?;
        }

        let state_machine = ConversationStateMachine::new_as_member();
        let initial_state = state_machine.current_state();

        // de-mls owns the outcome channel: subscribe the drain end, then move
        // the bus into the service that publishes resolutions into it. Storage
        // and signer are drawn from the backend (storage is the shared,
        // scope-keyed store).
        let outcome_bus = OutcomeBus::default();
        let consensus_rx = outcome_bus.subscribe();
        let consensus = ConsensusService::new_with_components(
            consensus.storage(),
            outcome_bus,
            consensus.signer(),
            config.max_consensus_sessions,
        );
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
            clock,
            Arc::from(self_member_id_bytes.as_slice()),
            app_id,
        );
        // Surface the opening phase so a caller draining conversation events sees
        // the conversation's starting state without a separate query.
        conversation.emit_event(ConversationEvent::PhaseChange(initial_state));
        Ok(conversation)
    }
}
