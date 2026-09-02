//! Read-only queries over a conversation's state.

use hashgraph_like_consensus::storage::ConsensusStorage;
use prost::Message;

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationState, MemberId, MemberRole,
    MlsGroup, PeerScoreStorage, StewardListSnapshot, WallClock, mls_crypto::member_id_of,
    protos::de_mls::messages::v1::ConversationUpdateRequest,
};

impl<Cp, Sc, Wc> Conversation<Cp, Sc, Wc>
where
    Cp: ConsensusPlugin,
    Sc: PeerScoreStorage,
    Wc: WallClock,
{
    /// Returns a reference to the underlying MLS group for this conversation,
    /// allowing read-only access to the group's state via the OpenMLS API.
    pub fn mls_group(&self) -> &MlsGroup {
        self.mls().group()
    }

    /// Current state of the conversation's state machine.
    pub fn state(&self) -> ConversationState {
        self.current_state()
    }

    /// Name of this conversation. Identifies it in the integrator's
    /// registry and on every [`crate::Outbound`] it produces.
    pub fn id(&self) -> &str {
        &self.conversation_id
    }

    /// The local member's `member_id` (leaf-index bytes) in this conversation.
    pub fn member_id_bytes(&self) -> &[u8] {
        &self.self_member_id
    }

    /// The local member's own [`MemberId`] handle.
    pub fn own_id(&self) -> MemberId {
        self.mls()
            .member_id_at(self.member_id_bytes())
            .expect("the local member is present in its own group")
    }

    /// App id this conversation tags on outbound packets and uses for self-echo
    /// filtering in [`Conversation::process_inbound`].
    pub fn app_id(&self) -> &[u8] {
        &self.app_id
    }

    /// Current MLS epoch + reelection retry round. Intended for UI status
    /// display.
    pub fn epoch_and_retry(&self) -> Result<(u64, u32), ConversationError> {
        let epoch = self.mls().group().epoch().as_u64();
        Ok((epoch, self.services.steward_list.next_election_round()))
    }

    /// Count of buffered pending membership updates. Used by tests and the UI
    /// to verify buffer hygiene (e.g., that a joiner's buffer is empty right
    /// after they receive the welcome).
    pub fn pending_update_count(&self) -> usize {
        self.queues.pending_update_count()
    }

    /// Commit round progress: `(received, expected)`. Returns `(0, 0)` if not
    /// in freeze or no steward list is known.
    pub fn commit_candidate_count(&self) -> (usize, usize) {
        let received = self.queues.commit_round.candidate_count();
        let expected = self
            .services
            .steward_list
            .current_list()
            .map(|l| l.len())
            .unwrap_or(0);
        (received, expected)
    }

    /// Whether this member holds a steward list it can use at the current
    /// epoch.
    ///
    /// False right after a welcome that carried no `ConversationSync`, and
    /// again once a list ages out before a newer election reaches this node.
    /// `poll` requests a fresh sync on its own in both cases, so this is a
    /// status to surface, not a condition to act on. Until it turns true this
    /// member has no steward list, no peer scores, and no protocol config of
    /// its own.
    pub fn is_synced(&self) -> Result<bool, ConversationError> {
        let epoch = self.mls().group().epoch().as_u64();
        Ok(match self.services.steward_list.current_list() {
            None => false,
            Some(_) => !self.services.steward_list.is_exhausted(epoch),
        })
    }

    /// Steward-list state a restart can't recompute. The list itself is
    /// deterministic; the election retry counters are not — persist this with
    /// whatever else you keep for the conversation. `Err` before a list is
    /// installed.
    pub fn steward_list_snapshot(&self) -> Result<StewardListSnapshot, ConversationError> {
        self.services.steward_list.snapshot()
    }

    /// Put back a [`Self::steward_list_snapshot`] from this same conversation.
    /// The sort salt is re-seeded from the live conversation, so a snapshot
    /// taken elsewhere won't generate the same subset elections.
    pub fn restore_steward_list(&mut self, snapshot: StewardListSnapshot) {
        self.services.steward_list.restore(snapshot);
        let conversation_id = self.conversation_id.clone();
        self.services
            .steward_list
            .set_conversation_id(conversation_id.as_bytes());
    }

    pub fn is_steward(&self) -> bool {
        self.services.steward_list.is_steward(&self.self_member_id)
    }

    /// Returns `true` if this member is the main (primary) steward for the current epoch.
    /// Only one member is the primary at a time; others are backups.
    /// The primary handles joiners and runs certain actions first.
    /// Backups wait their turn and only take over if the primary is silent for a while.
    /// Use [`Self::is_steward`] if you want to know if you are any steward (primary or backup).
    pub fn is_epoch_steward(&self) -> Result<bool, ConversationError> {
        let mls = self.mls();
        let epoch = mls.group().epoch().as_u64();
        let members = mls.members()?;
        let eligible = self.queues.steward_eligibility(&members);
        let (epoch_steward, _backup) = self
            .services
            .steward_list
            .epoch_and_backup(epoch, &eligible);
        Ok(epoch_steward == Some(self.self_member_id.as_ref()))
    }

    /// Score at or below which a member is eligible for removal
    /// (RFC §Peer Scoring `threshold_peer_score`), adopted from
    /// `ConversationSync` at join so members share the line.
    pub fn score_threshold(&self) -> i64 {
        self.services.scoring.threshold()
    }

    /// Score a member starts at (RFC §Peer Scoring `default_peer_score`), also
    /// adopted from `ConversationSync`. Pairs with [`Self::score_threshold`] to
    /// give the range a score sits in, which is what an application needs to
    /// render one or band it — neither is knowable from the local config once
    /// the group's values are in force.
    pub fn score_default(&self) -> i64 {
        self.services.scoring.default_score()
    }

    /// Peer score of every current member, keyed by its [`MemberId`] handle.
    pub fn member_scores(&self) -> Result<Vec<(MemberId, i64)>, ConversationError> {
        let mls = self.mls();
        Ok(self
            .services
            .scoring
            .all_members_with_scores()?
            .into_iter()
            .filter_map(|(id, score)| Some((mls.member_id_at(&id)?, score)))
            .collect())
    }

    /// Peer score for `member`, `None` if unscored; `MemberGone` if the handle
    /// is stale.
    pub fn member_score(&self, member: &MemberId) -> Result<Option<i64>, ConversationError> {
        let leaf = self
            .mls()
            .leaf_of(member)
            .ok_or(ConversationError::MemberGone)?;
        self.services.scoring.score_for(&member_id_of(leaf))
    }

    /// Members that have an in-flight self-leave request, by [`MemberId`].
    pub fn pending_leave(&self) -> Result<Vec<MemberId>, ConversationError> {
        let mls = self.mls();
        Ok(mls
            .members()?
            .into_iter()
            .filter(|id| self.queues.is_pending_self_leave(id))
            .filter_map(|id| mls.member_id_at(&id))
            .collect())
    }

    /// Steward role of each member, keyed by its [`MemberId`] handle. Uses live
    /// rotation so removed or pending-leave stewards are skipped in role display.
    pub fn member_roles(&self) -> Result<Vec<(MemberId, MemberRole)>, ConversationError> {
        let mls = self.mls();
        let epoch = mls.group().epoch().as_u64();
        let members = mls.members()?;

        let eligible = self.queues.steward_eligibility(&members);
        let (live_epoch, live_backup) = self
            .services
            .steward_list
            .epoch_and_backup(epoch, &eligible);
        let live_epoch = live_epoch.map(|s| s.to_vec());
        let live_backup = live_backup.map(|s| s.to_vec());
        let exhausted = self.services.steward_list.is_exhausted(epoch);
        let has_list = self.services.steward_list.current_list().is_some();
        let roles = members
            .iter()
            .cloned()
            .filter_map(|id| {
                let role = if has_list && !exhausted {
                    if live_epoch.as_deref().is_some_and(|es| es == id) {
                        MemberRole::EpochSteward
                    } else if live_backup.as_deref().is_some_and(|bs| bs == id) {
                        MemberRole::BackupSteward
                    } else if self.services.steward_list.is_steward(&id) {
                        MemberRole::Steward
                    } else {
                        MemberRole::Member
                    }
                } else if has_list && self.services.steward_list.is_steward(&id) {
                    MemberRole::Steward
                } else {
                    MemberRole::Member
                };
                Some((mls.member_id_at(&id)?, role))
            })
            .collect();
        Ok(roles)
    }

    /// Proposals approved for the current epoch, each with the `proposal_id`
    /// that identified it in consensus.
    pub fn approved_proposals_for_current_epoch(&self) -> Vec<(u32, ConversationUpdateRequest)> {
        self.queues
            .approved_proposals()
            .iter()
            .map(|(id, req)| (*id, req.clone()))
            .collect()
    }

    /// What `proposal_id` asked for, as carried in the consensus session.
    ///
    /// Resolves the id an [`crate::Info::ConsensusReached`] carries.
    /// `None` once the session has been freed — sessions are
    /// retained up to `max_consensus_sessions`, so old ids age out.
    pub fn proposal(
        &self,
        proposal_id: u32,
    ) -> Result<Option<ConversationUpdateRequest>, ConversationError> {
        let scope = self.conversation_id.clone();
        let Some(session) = self
            .services
            .consensus
            .storage()
            .get_session(&scope, proposal_id)?
        else {
            return Ok(None);
        };
        Ok(Some(ConversationUpdateRequest::decode(
            session.proposal.payload.as_slice(),
        )?))
    }
}
