//! [`User`] struct definition, constructor, accessors, and the
//! consensus-context helpers shared across the User submodules
//! (`lifecycle`, `inbound`, `registry`).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use crate::{
    app::{
        ConversationState, CreatorVote, LockExt, MemberRole, SessionRunner, SessionTick, UserError,
        UserPlugins,
    },
    core::{
        ConsensusPlugin, ConsensusServiceFor, ConversationLifecycle, ConversationPluginsFactory,
        ScoringConfig, SessionEvent, StewardListConfig,
    },
    ds::SharedDeliveryService,
    member_id::MemberId,
    mls_crypto::{KeyPackageBytes, MlsError, MlsService, key_package_bytes_from_tls},
    protos::de_mls::messages::v1::{
        BanRequest, ConversationUpdateRequest, MemberInvite, conversation_update_request,
    },
};

/// Single registry entry: one `Arc<RwLock<SessionRunner>>` per conversation.
/// Cloned out of the registry under the outer read lock, then locked
/// independently — writes on one conversation don't block reads on another.
pub type SessionEntry<P, CP> = Arc<RwLock<SessionRunner<P, CP>>>;

/// Per-user registry of conversation runners. Each entry's inner per-runner
/// lock guards per-conversation reads/mutations so a write on conversation
/// A doesn't block reads on conversation B.
pub(crate) type ConversationRegistry<P, CP> = RwLock<HashMap<String, SessionEntry<P, CP>>>;

pub struct User<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    pub(crate) member_id: Box<dyn MemberId>,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped.
    pub(crate) app_id: Vec<u8>,
    /// Synchronous outbound transport. Cloned into each `SessionRunner` at
    /// construction. Stored behind a `Mutex` because the trait takes
    /// `&mut self`.
    pub(crate) transport: SharedDeliveryService,
    /// All User-level plugin state: the per-conversation factory, the
    /// consensus context, the key-package provider, and the three default
    /// configs cloned into newly-created sessions.
    pub(crate) plugins: UserPlugins<P, CP>,
    /// Per-conversation `SessionRunner`s.
    pub(crate) conversations: ConversationRegistry<P, CP>,
    /// User-level conversation lifecycle events: `Created(name)` /
    /// `Removed(name)`. Integrators drain via
    /// [`Self::drain_lifecycle_events`] once per polling cycle to learn
    /// when new sessions appear and old ones disappear. Interior `Mutex`
    /// so producer-side methods stay `&self`.
    pub(crate) pending_lifecycle_events: Mutex<Vec<ConversationLifecycle>>,
}

// ── Public API ──────────────────────────────────────────────────────────

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Display form of the local member_id, derived from [`MemberId::member_id_display`].
    pub fn member_id_string(&self) -> String {
        self.member_id.member_id_display().to_string()
    }

    /// Identity bytes of the local user, via the [`MemberId`] trait.
    pub fn member_id_bytes(&self) -> &[u8] {
        self.member_id.member_id_bytes()
    }

    /// Per-instance `app_id` embedded in every outbound packet. Inbound
    /// packets carrying this `app_id` are self-echoes and are dropped
    /// by [`Self::process_inbound_packet`].
    pub fn app_id(&self) -> &[u8] {
        &self.app_id
    }

    /// Generate a single-use key package.
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        self.plugins.conversation_plugins.generate_key_package()
    }

    /// Drain every pending [`ConversationLifecycle`] event accumulated
    /// since the last call. Returns events in insertion order. Callers
    /// (gateway, integrator) invoke this once per polling cycle to discover
    /// `Created` / `Removed` sessions and wire up per-session event
    /// drains via [`SessionRunner::drain_events`].
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
    /// [`SessionRunner::send_app_message`]. Errors with
    /// `ConversationNotFound` if the conversation has been removed, or
    /// `ConversationBlocked` if the session is gating chat traffic.
    pub async fn send_app_message(
        &self,
        conversation_id: &str,
        message: Vec<u8>,
    ) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::send_app_message(&entry, message).await
    }

    /// Broadcast `key_package` on `conversation_id`'s welcome subtopic
    /// so existing members can propose adding us. Thin wrapper over
    /// [`SessionRunner::send_key_package`].
    pub async fn send_key_package(
        &self,
        conversation_id: &str,
        key_package: KeyPackageBytes,
    ) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::send_key_package(&entry, key_package).await
    }

    /// Walk pending deadlines on `conversation_id`. Thin wrapper over
    /// [`SessionRunner::tick_deadlines`].
    pub async fn tick_deadlines(&self, conversation_id: &str) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::tick_deadlines(&entry).await
    }

    /// Advance every per-conversation polling path: deadlines (auto-votes
    /// and consensus timeouts), freeze coordinator, member-side freeze
    /// check, and `PendingJoin` expiry. Bundled so an integrator's
    /// periodic wakeup needs one call to drive a session forward.
    ///
    /// The returned `SessionTick.next_wakeup_in` covers every time-based
    /// transition the session is currently expecting. Drained events
    /// (e.g. `Leaving` on `PendingJoin` expiry) are read separately via
    /// [`Self::drain_events`].
    pub async fn poll_session(&self, conversation_id: &str) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::tick_deadlines(&entry).await?;
        SessionRunner::poll_freeze_status(&entry).await?;
        SessionRunner::check_member_freeze(&entry).await?;
        SessionRunner::check_pending_join(&entry)?;
        Ok(entry.read_or_err("session")?.tick())
    }

    /// Drain pending [`SessionEvent`]s for `conversation_id`. Thin
    /// wrapper over [`SessionRunner::drain_events`].
    pub fn drain_events(&self, conversation_id: &str) -> Result<Vec<SessionEvent>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.drain_events())
    }

    /// Earliest pending deadline on `conversation_id` relative to now,
    /// `None` if nothing is scheduled. Mirrors
    /// [`SessionRunner::next_wakeup_in`].
    pub fn next_wakeup_in(&self, conversation_id: &str) -> Result<Option<Duration>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.next_wakeup_in())
    }

    /// Propose adding the holder of `key_package_bytes` to
    /// `conversation_id`. The local vote is bundled YES at submit. On
    /// consensus YES the epoch steward authors a commit containing the
    /// Add; the resulting welcome arrives via
    /// [`crate::core::SessionEvent::WelcomeReady`] for the integrator to
    /// deliver out of band.
    pub async fn add_member(
        &self,
        conversation_id: &str,
        key_package_bytes: &[u8],
    ) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        let (key_package_bytes, member_id) =
            key_package_bytes_from_tls(key_package_bytes.to_vec())?;
        let request = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::MemberInvite(
                MemberInvite {
                    key_package_bytes,
                    member_id,
                },
            )),
        };
        SessionRunner::initiate_proposal(&entry, request, CreatorVote::Yes).await?;
        Ok(entry.read_or_err("session")?.tick())
    }

    /// Ingest a raw MLS welcome blob delivered out of band (e.g. the
    /// inviter's [`crate::core::SessionEvent::WelcomeReady`] routed
    /// through the integrator's transport). Returns the joined
    /// conversation name, or [`UserError::WelcomeNotForUs`] if the
    /// welcome doesn't address this user's key package.
    pub async fn accept_welcome(
        &mut self,
        welcome_bytes: &[u8],
    ) -> Result<(String, SessionTick), UserError> {
        let svc = self
            .plugins
            .conversation_plugins
            .welcome_mls(welcome_bytes)?
            .ok_or(UserError::WelcomeNotForUs)?;
        let conversation_id = svc.conversation_id().to_string();

        if self.lookup_entry(&conversation_id)?.is_none() {
            self.start_conversation(&conversation_id, false).await?;
        }
        let entry_arc = self
            .lookup_entry(&conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;

        {
            let mut entry = entry_arc.write_or_err("session")?;
            // Idempotency keys off the state machine, not MLS attachment:
            // past `PendingJoin` means the join dispatch already finished.
            // Still-`PendingJoin`-with-MLS is a join that failed after
            // `attach_mls`; fall through to finish it. Re-attach only when
            // absent — overwriting would drop the existing group state.
            if entry.conversation.current_state() != ConversationState::PendingJoin {
                return Ok((conversation_id, entry.tick()));
            }
            if entry.conversation.mls().is_none() {
                entry.conversation.attach_mls(svc);
            }
        }
        self.finish_dispatch(
            &conversation_id,
            &entry_arc,
            crate::core::ProcessResult::JoinedConversation(conversation_id.clone()),
        )
        .await?;
        let tick = entry_arc.read_or_err("session")?.tick();
        Ok((conversation_id, tick))
    }

    // ── UI actions ─────────────────────────────────────────────────────

    /// Cast the local member's manual vote on `proposal_id`. Cancels any
    /// pending auto-vote so the manual choice wins. Thin wrapper over
    /// [`SessionRunner::process_user_vote`].
    pub async fn process_user_vote(
        &self,
        conversation_id: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::process_user_vote(&entry, proposal_id, vote).await?;
        Ok(entry.read_or_err("session")?.tick())
    }

    /// Open a `RemoveMember` consensus round targeting
    /// `ban_request.user_to_ban`. The local vote is bundled YES at submit.
    /// Thin wrapper over [`SessionRunner::process_ban_request`].
    pub async fn process_ban_request(
        &self,
        conversation_id: &str,
        ban_request: BanRequest,
    ) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::process_ban_request(&entry, ban_request).await
    }

    /// Submit `request` as a fresh consensus proposal with
    /// `creator_vote`. Lower-level than
    /// [`Self::add_member`] / [`Self::process_ban_request`]; use those
    /// for membership changes. Thin wrapper over
    /// [`SessionRunner::initiate_proposal`].
    pub async fn initiate_proposal(
        &self,
        conversation_id: &str,
        request: ConversationUpdateRequest,
        creator_vote: CreatorVote,
    ) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::initiate_proposal(&entry, request, creator_vote).await?;
        Ok(entry.read_or_err("session")?.tick())
    }

    /// Open a self-leave consensus round for the local member. The
    /// resulting commit ejects us; the session emits
    /// [`SessionEvent::Leaving`] and the caller follows up with
    /// [`Self::finalize_self_leave`]. Thin wrapper over
    /// [`SessionRunner::initiate_self_leave`].
    pub async fn initiate_self_leave(
        &self,
        conversation_id: &str,
    ) -> Result<SessionTick, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::initiate_self_leave(&entry).await?;
        Ok(entry.read_or_err("session")?.tick())
    }

    // ── State queries ──────────────────────────────────────────────────

    /// Current state-machine value for `conversation_id`. Mirrors
    /// [`SessionRunner::get_conversation_state`].
    pub fn get_conversation_state(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationState, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_conversation_state())
    }

    /// `true` if the local user is on the current steward list for
    /// `conversation_id`. Mirrors [`SessionRunner::is_steward_for_self`].
    pub fn is_steward(&self, conversation_id: &str) -> Result<bool, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.is_steward_for_self())
    }

    /// MLS epoch + reelection retry round for `conversation_id`. `(0, 0)`
    /// before MLS is attached. Mirrors
    /// [`SessionRunner::get_epoch_and_retry`].
    pub fn get_epoch_and_retry(&self, conversation_id: &str) -> Result<(u64, u32), UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry.read_or_err("session")?.get_epoch_and_retry()
    }

    pub fn get_conversation_members(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Vec<u8>>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry.read_or_err("session")?.get_conversation_members()
    }

    pub fn get_member_scores(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<(Vec<u8>, i64)>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_member_scores())
    }

    pub fn get_member_score(
        &self,
        conversation_id: &str,
        member_id: &[u8],
    ) -> Result<Option<i64>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_member_score(member_id))
    }

    pub fn get_member_roles(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<(Vec<u8>, MemberRole)>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry.read_or_err("session")?.get_member_roles()
    }

    /// Identities with an in-flight self-leave request. Mirrors
    /// [`SessionRunner::get_pending_leave_member_ids`].
    pub fn get_pending_leave_member_ids(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Vec<u8>>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry.read_or_err("session")?.get_pending_leave_member_ids()
    }

    /// Buffered pending membership-update count. Mirrors
    /// [`SessionRunner::get_pending_update_count`].
    pub fn get_pending_update_count(&self, conversation_id: &str) -> Result<usize, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_pending_update_count())
    }

    /// Freeze-round progress as `(received, expected)`. Mirrors
    /// [`SessionRunner::get_freeze_candidate_count`].
    pub fn get_freeze_candidate_count(
        &self,
        conversation_id: &str,
    ) -> Result<(usize, usize), UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_freeze_candidate_count())
    }

    /// Approved proposals for the current epoch. Mirrors
    /// [`SessionRunner::get_approved_proposals_for_current_epoch`].
    pub fn get_approved_proposals_for_current_epoch(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ConversationUpdateRequest>, UserError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        Ok(entry
            .read_or_err("session")?
            .get_approved_proposals_for_current_epoch())
    }
}

// ── User-internal helpers ───────────────────────────────────────────────

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    pub fn self_member_id(&self) -> &[u8] {
        self.member_id.member_id_bytes()
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

    /// Build a fresh per-conversation `ConsensusService` via the User's
    /// `ConsensusContext`.
    pub fn build_consensus_service(&self) -> ConsensusServiceFor<P> {
        self.plugins.consensus.build_service()
    }

    /// Drop this conversation's consensus scope from the shared storage and
    /// clear every auto-vote registered for it. Called on leave and
    /// pending-join timeout.
    pub async fn cleanup_consensus_scope(&self, conversation_id: &str) -> Result<(), UserError> {
        if let Some(entry_arc) = self.lookup_entry(conversation_id)? {
            entry_arc
                .write()
                .map_err(|_| UserError::LockPoisoned("session"))?
                .cancel_all_auto_votes();
        }
        let scope = P::Scope::from(conversation_id.to_string());
        self.plugins.consensus.delete_scope(&scope).await?;
        Ok(())
    }

    pub fn new_with_plugins(
        member_id: Box<dyn MemberId>,
        plugins: UserPlugins<P, CP>,
        transport: SharedDeliveryService,
    ) -> Self {
        Self {
            member_id,
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            transport,
            plugins,
            conversations: RwLock::new(HashMap::new()),
            pending_lifecycle_events: Mutex::new(Vec::new()),
        }
    }
}
