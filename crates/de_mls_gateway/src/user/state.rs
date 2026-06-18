//! [`User`] struct definition, constructor, accessors, and the
//! consensus-context helpers shared across the User submodules
//! (`lifecycle`, `inbound`, `registry`).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use de_mls::{
    ConsensusPlugin, Conversation, ConversationConfig, ConversationDeps, ConversationError,
    ConversationEvent, ConversationPluginsFactory, ConversationState, CreatorVote, MemberRole,
    PollOutcome, ScoringConfig, StewardListConfig,
    member_id::MemberId,
    mls_crypto::{KeyPackageBytes, MlsError},
    protos::de_mls::messages::v1::{ConversationUpdateRequest, MemberWelcome},
};
use de_mls_ds::{OutboundPacket, SharedDeliveryService};
use openmls_traits::signatures::Signer;

use crate::mls::DefaultConversationPluginsFactory;
use crate::user::{LockExt, UserError, UserPlugins};

/// Registry-level notification emitted when conversations are created or
/// removed. Drain via [`User::drain_lifecycle_events`] once per polling cycle.
#[derive(Debug, Clone)]
pub enum ConversationLifecycle {
    /// A new entry has been registered. The conversation is in the registry;
    /// the integrator can look it up and begin draining its events.
    Created(String),
    /// An entry has been removed from the registry.
    Removed(String),
}

/// Single registry entry: one `Arc<RwLock<Conversation>>` per conversation.
/// Cloned out of the registry under the outer read lock, then locked
/// independently — writes on one conversation don't block reads on another.
pub type ConversationEntry<P, CP> = Arc<RwLock<Conversation<P, CP>>>;

/// Per-user registry of conversations. Each entry's inner per-conversation
/// lock guards per-conversation reads/mutations so a write on conversation
/// A doesn't block reads on conversation B.
pub(crate) type ConversationRegistry<P, CP> = RwLock<HashMap<String, ConversationEntry<P, CP>>>;

pub struct User<P: ConsensusPlugin, CP: ConversationPluginsFactory, Sig: Signer> {
    pub(crate) member_id: Box<dyn MemberId>,
    /// MLS signing key for this user. Owned here — the single holder across
    /// the conversation registry — and passed by reference into every
    /// conversation-driving call that needs to sign.
    pub(crate) signer: Sig,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped.
    pub(crate) app_id: Vec<u8>,
    /// Synchronous outbound transport. The conversations are pull-only and never
    /// send; [`Self::flush`] drains a conversation's buffered outbound and
    /// publishes it here. Behind a `Mutex` because the trait takes `&mut self`.
    pub(crate) transport: SharedDeliveryService,
    /// All User-level plugin state: the per-conversation factory, the
    /// consensus context, the key-package provider, and the three default
    /// configs cloned into newly-created conversations.
    pub(crate) plugins: UserPlugins<P, CP>,
    /// Conversation handles keyed by conversation id.
    pub(crate) conversations: ConversationRegistry<P, CP>,
    /// User-level conversation lifecycle events: `Created(name)` /
    /// `Removed(name)`. Integrators drain via
    /// [`Self::drain_lifecycle_events`] once per polling cycle to learn
    /// when new conversations appear and old ones disappear. Interior `Mutex`
    /// so producer-side methods stay `&self`.
    pub(crate) pending_lifecycle_events: Mutex<Vec<ConversationLifecycle>>,
}

// ── Public API ──────────────────────────────────────────────────────────

impl<P: ConsensusPlugin, Sig: Signer> User<P, DefaultConversationPluginsFactory, Sig> {
    /// Generate a single-use key package via the default factory. Key-package
    /// generation is the integrator's concern — not part of the de-mls plug-in
    /// contract — so it lives on the concrete-factory `User`.
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        self.plugins.conversation_plugins.generate_key_package()
    }
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory, Sig: Signer + Clone> User<P, CP, Sig> {
    /// Display form of the local member_id, derived from [`MemberId::member_id_display`].
    pub fn member_id_string(&self) -> String {
        self.member_id.member_id_display().to_string()
    }

    /// Identity bytes of the local user, via the [`MemberId`] trait.
    pub fn member_id_bytes(&self) -> &[u8] {
        self.member_id.member_id_bytes()
    }

    /// Per-instance `app_id` stamped on every outbound. Inbound carrying
    /// this `app_id` is a self-echo and is dropped by
    /// [`Self::handle_inbound`].
    pub fn app_id(&self) -> &[u8] {
        &self.app_id
    }

    /// Drain every pending [`ConversationLifecycle`] event accumulated
    /// since the last call. Returns events in insertion order. Callers
    /// (gateway, integrator) invoke this once per polling cycle to discover
    /// `Created` / `Removed` conversations and wire up per-conversation event
    /// drains via [`Conversation::drain_events`].
    pub fn drain_lifecycle_events(&self) -> Vec<ConversationLifecycle> {
        match self.pending_lifecycle_events.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => {
                tracing::error!(
                    "lifecycle-event buffer mutex poisoned; integrator will miss Created/Removed events"
                );
                Vec::new()
            }
        }
    }

    /// Override the seed [`ScoringConfig`] used for newly-created per-conversation
    /// scoring plug-ins. Existing conversations are untouched; their plug-ins
    /// already own their live config (joiner-side overwritten by ConversationSync).
    pub fn set_default_scoring_config(&mut self, config: ScoringConfig) {
        self.plugins.default_scoring_config = config;
    }

    /// Override the seed [`StewardListConfig`] used for newly-created
    /// per-conversation steward-list plug-ins. Same lifecycle as
    /// [`Self::set_default_scoring_config`].
    pub fn set_default_steward_list_config(&mut self, config: StewardListConfig) {
        self.plugins.default_steward_list_config = config;
    }

    /// Send a chat message on `conversation_id`. Thin wrapper over
    /// [`Conversation::send_message`]. Errors with
    /// `ConversationNotFound` if the conversation has been removed, or
    /// `ConversationBlocked` if the conversation is gating chat traffic.
    pub fn send_message(&self, conversation_id: &str, message: Vec<u8>) -> Result<(), UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry
            .write_or_err("conversation")?
            .send_message(message, &self.signer)?;
        self.flush(&entry)?;
        Ok(())
    }

    /// Announce `key_package` on `conversation_id` so existing members can
    /// propose adding us. Key-package creation is a user-level concern — the
    /// conversation knows nothing about how a key package is built — so this
    /// builds the announcement and publishes it straight to the transport,
    /// bypassing the conversation entirely.
    pub fn send_key_package(
        &self,
        conversation_id: &str,
        key_package: KeyPackageBytes,
    ) -> Result<(), UserError> {
        let payload = de_mls::build_key_package_announcement(&key_package);
        let packet = OutboundPacket::key_package(conversation_id, &self.app_id, payload);
        self.transport
            .lock()
            .map_err(|_| UserError::LockPoisoned("transport"))?
            .publish(packet)
            .map_err(|e| UserError::Transport(e.to_string()))?;
        Ok(())
    }

    /// Drive one polling cycle on `conversation_id`: tick consensus deadlines,
    /// advance freeze state, and check member-freeze inactivity. Returns
    /// [`PollOutcome`] with a wakeup hint and a `leave_requested` flag the
    /// caller uses to decide whether to finalize the leave.
    pub fn poll_conversation(&self, conversation_id: &str) -> Result<PollOutcome, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        let outcome = entry.write_or_err("conversation")?.poll(&self.signer);
        self.flush(&entry)?;
        Ok(outcome)
    }

    /// Drain pending [`ConversationEvent`]s for `conversation_id`. Thin
    /// wrapper over [`Conversation::drain_events`].
    pub fn drain_events(&self, conversation_id: &str) -> Result<Vec<ConversationEvent>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.drain_events())
    }

    /// Earliest pending deadline on `conversation_id` relative to now,
    /// `None` if nothing is scheduled. Mirrors
    /// [`Conversation::next_wakeup_in`].
    pub fn next_wakeup_in(&self, conversation_id: &str) -> Result<Option<Duration>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.next_wakeup_in())
    }

    /// Propose adding the holder of `key_package_bytes` to
    /// `conversation_id`. The local vote is bundled YES at submit. On
    /// consensus YES the epoch steward authors a commit containing the
    /// Add; the resulting welcome arrives via
    /// [`de_mls::ConversationEvent::WelcomeReady`] for the integrator to
    /// deliver out of band.
    pub fn add_member(
        &self,
        conversation_id: &str,
        key_package_bytes: &[u8],
    ) -> Result<(), UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry
            .write_or_err("conversation")?
            .add_member(key_package_bytes, &self.signer)?;
        self.flush(&entry)?;
        Ok(())
    }

    /// Ingest a [`MemberWelcome`] delivered out of band (e.g. the
    /// inviter's [`de_mls::ConversationEvent::WelcomeReady`] routed
    /// through the integrator's transport). Builds a fully-joined
    /// conversation from the welcome — seeding MLS, running the joiner-side
    /// side-effects, and replaying the bundled `ConversationSync` — and
    /// registers it. Returns the joined conversation name, or
    /// [`UserError::WelcomeNotForUs`] if the welcome doesn't address this
    /// user's key package. Idempotent: a welcome for an already-joined
    /// conversation returns its name without re-registering.
    pub fn accept_welcome(&mut self, welcome: &MemberWelcome) -> Result<String, UserError> {
        let deps = self.build_deps(self.plugins.default_conversation_config.clone());
        let Some(conversation) = Conversation::from_welcome(deps, welcome, &self.signer)? else {
            return Err(UserError::WelcomeNotForUs);
        };
        let conversation_id = conversation.id().to_string();
        if self.lookup_entry(&conversation_id)?.is_some() {
            return Ok(conversation_id);
        }
        let entry_arc = self.register_built(&conversation_id, conversation)?;
        self.flush(&entry_arc)?;
        Ok(conversation_id)
    }

    // ── UI actions ─────────────────────────────────────────────────────

    /// Cast the local member's manual vote on `proposal_id`. Cancels any
    /// pending auto-vote so the manual choice wins. Thin wrapper over
    /// [`Conversation::vote`].
    pub fn vote(
        &self,
        conversation_id: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry
            .write_or_err("conversation")?
            .vote(proposal_id, vote, &self.signer)?;
        self.flush(&entry)?;
        Ok(())
    }

    /// Open a `RemoveMember` consensus round targeting `member_id`. The
    /// local vote is bundled YES at submit. Thin wrapper over
    /// [`Conversation::remove_member`].
    pub fn remove_member(&self, conversation_id: &str, member_id: &[u8]) -> Result<(), UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry
            .write_or_err("conversation")?
            .remove_member(member_id, &self.signer)?;
        self.flush(&entry)?;
        Ok(())
    }

    /// Submit `request` as a fresh consensus proposal with
    /// `creator_vote`. Lower-level than
    /// [`Self::add_member`] / [`Self::remove_member`]; use those
    /// for membership changes. Thin wrapper over
    /// [`Conversation::initiate_proposal`].
    pub fn initiate_proposal(
        &self,
        conversation_id: &str,
        request: ConversationUpdateRequest,
        creator_vote: CreatorVote,
    ) -> Result<(), UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry.write_or_err("conversation")?.initiate_proposal(
            request,
            creator_vote,
            &self.signer,
        )?;
        self.flush(&entry)?;
        Ok(())
    }

    // ── State queries ──────────────────────────────────────────────────

    /// Current state-machine value for `conversation_id`. Mirrors
    /// [`Conversation::state`].
    pub fn conversation_state(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationState, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.state())
    }

    /// `true` if the local user is on the current steward list for
    /// `conversation_id`. Mirrors [`Conversation::is_steward`].
    pub fn is_steward(&self, conversation_id: &str) -> Result<bool, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.is_steward())
    }

    /// MLS epoch + reelection retry round for `conversation_id`. Mirrors
    /// [`Conversation::epoch_and_retry`].
    pub fn epoch_and_retry(&self, conversation_id: &str) -> Result<(u64, u32), UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.epoch_and_retry()?)
    }

    pub fn members(&self, conversation_id: &str) -> Result<Vec<Vec<u8>>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.members()?)
    }

    pub fn member_scores(&self, conversation_id: &str) -> Result<Vec<(Vec<u8>, i64)>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.member_scores())
    }

    pub fn member_score(
        &self,
        conversation_id: &str,
        member_id: &[u8],
    ) -> Result<Option<i64>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.member_score(member_id))
    }

    pub fn member_roles(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<(Vec<u8>, MemberRole)>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.member_roles()?)
    }

    /// Identities with an in-flight self-leave request. Mirrors
    /// [`Conversation::pending_leave_member_ids`].
    pub fn pending_leave_member_ids(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Vec<u8>>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry
            .read_or_err("conversation")?
            .pending_leave_member_ids()?)
    }

    /// Buffered pending membership-update count. Mirrors
    /// [`Conversation::pending_update_count`].
    pub fn pending_update_count(&self, conversation_id: &str) -> Result<usize, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("conversation")?.pending_update_count())
    }

    /// Approved proposals for the current epoch. Mirrors
    /// [`Conversation::approved_proposals_for_current_epoch`].
    pub fn approved_proposals_for_current_epoch(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ConversationUpdateRequest>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry
            .read_or_err("conversation")?
            .approved_proposals_for_current_epoch())
    }
}

// ── User-internal helpers ───────────────────────────────────────────────

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory, Sig: Signer> User<P, CP, Sig> {
    pub fn self_member_id(&self) -> &[u8] {
        self.member_id.member_id_bytes()
    }

    /// Assemble a [`ConversationDeps`] bundle for a new conversation on this
    /// user — the shared plug-in factory, a freshly-minted consensus service,
    /// the local identity, and the per-conversation configs. Shared by the
    /// creator path (`register_conversation`) and the welcome-driven join
    /// (`accept_welcome`).
    pub(crate) fn build_deps(&self, config: ConversationConfig) -> ConversationDeps<'_, P, CP> {
        ConversationDeps {
            plugins: &self.plugins.conversation_plugins,
            consensus: self.plugins.consensus.build_service(),
            identity: self.member_id.as_ref(),
            app_id: Arc::from(self.app_id.as_slice()),
            config,
            scoring_config: self.plugins.default_scoring_config.clone(),
            steward_list_config: self.plugins.default_steward_list_config.clone(),
        }
    }

    /// Insert an already-built [`Conversation`] into the registry, emit the
    /// `Created` lifecycle event, and return the registry entry. Errors with
    /// [`UserError::ConversationAlreadyExists`] if an entry races in under the
    /// write lock. Shared by the creator path and the welcome-driven join.
    pub(crate) fn register_built(
        &self,
        conversation_id: &str,
        conversation: Conversation<P, CP>,
    ) -> Result<ConversationEntry<P, CP>, UserError> {
        let entry = Arc::new(RwLock::new(conversation));
        {
            let mut conversations = self
                .conversations
                .write()
                .map_err(|_| UserError::LockPoisoned("conversation registry"))?;
            if conversations.contains_key(conversation_id) {
                return Err(UserError::ConversationAlreadyExists);
            }
            conversations.insert(conversation_id.to_string(), entry.clone());
        }
        // The conversation already buffered its opening `PhaseChange`; record the
        // lifecycle event so integrators draining
        // [`Self::drain_lifecycle_events`] discover the conversation.
        self.emit_lifecycle(ConversationLifecycle::Created(conversation_id.to_string()));
        Ok(entry)
    }

    /// Append a [`ConversationLifecycle`] event to the pending-events buffer
    /// for [`Self::drain_lifecycle_events`]. Fire-and-forget (no `Result`),
    /// but a poisoned buffer is logged rather than silently dropped.
    pub fn emit_lifecycle(&self, event: ConversationLifecycle) {
        match self.pending_lifecycle_events.lock() {
            Ok(mut buf) => buf.push(event),
            Err(_) => tracing::error!(
                ?event,
                "lifecycle-event buffer mutex poisoned; event dropped"
            ),
        }
    }

    /// Publish a conversation's buffered outbound on the User's transport. The
    /// conversation is pull-only — it buffers packets and never sends; this is the
    /// User's push-adapter so its callers keep their send-on-op behaviour.
    /// (When `User` moves out of the library, the integrator drains and
    /// publishes directly instead.)
    pub(crate) fn flush(&self, entry: &ConversationEntry<P, CP>) -> Result<(), UserError> {
        let outbound = entry.read_or_err("conversation")?.drain_outbound();
        if outbound.is_empty() {
            return Ok(());
        }
        let mut transport = self
            .transport
            .lock()
            .map_err(|_| UserError::LockPoisoned("transport"))?;
        for out in outbound {
            transport
                .publish(out.into())
                .map_err(|e| UserError::Transport(e.to_string()))?;
        }
        Ok(())
    }

    /// Drop this conversation's consensus scope from the shared storage.
    /// Called on leave, after the conversation has already cancelled its own
    /// auto-votes.
    pub fn cleanup_consensus_scope(&self, conversation_id: &str) -> Result<(), UserError> {
        let scope = P::Scope::from(conversation_id.to_string());
        self.plugins
            .consensus
            .delete_scope(&scope)
            .map_err(ConversationError::from)?;
        Ok(())
    }

    pub fn new_with_plugins(
        member_id: Box<dyn MemberId>,
        signer: Sig,
        plugins: UserPlugins<P, CP>,
        transport: SharedDeliveryService,
    ) -> Self {
        Self {
            member_id,
            signer,
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            transport,
            plugins,
            conversations: RwLock::new(HashMap::new()),
            pending_lifecycle_events: Mutex::new(Vec::new()),
        }
    }
}
