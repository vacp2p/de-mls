//! Building an [`Engine`] from the facts the router holds: the conversation
//! id, this member, and the group's epoch and member set.

use tracing::warn;

use crate::{
    ConversationError,
    engine::{
        config::EngineConfig,
        handle::Engine,
        store::{Dirty, EngineStore, keys},
        types::{Event, MemberId, Output, Timestamp},
    },
};

impl<St: EngineStore> Engine<St> {
    /// Start a conversation this member created. `members` is the group's
    /// member set at `epoch`, `own` included. A single member is a plain
    /// creation; several are a founding group the router seated in one
    /// commit before calling, all of them settled stewards from `epoch`.
    pub fn create(
        conversation_id: &str,
        own: MemberId,
        epoch: u64,
        members: &[MemberId],
        config: EngineConfig,
        store: St,
    ) -> Result<(Self, Output), ConversationError> {
        if !members.contains(&own) {
            return Err(ConversationError::EmptyMembersList);
        }
        let mut engine = Self::assemble(conversation_id, own, epoch, members, config, store)?;
        engine.begin(Timestamp::ZERO);
        for member in engine.members.clone() {
            engine.scoring.add_member(&member);
        }
        let settled = engine.members.clone();
        let sn = engine
            .steward_list
            .config()
            .compute_list_size(settled.len());
        engine.steward_list.install_list(epoch, &settled, sn, 0)?;
        let phase = engine.phase();
        engine.emit(Event::PhaseChange(phase));
        // A fresh store: every key gets its first write.
        engine.dirty = Dirty::ALL;
        let out = engine.finish()?;
        Ok((engine, out))
    }

    /// Start a conversation this member joined from a welcome. The welcome
    /// carries nothing but the group state: the engine starts unsynced and
    /// adopts the first `ConversationSync` a steward broadcasts, falling
    /// back to `request_sync` if none arrives.
    pub fn join(
        conversation_id: &str,
        own: MemberId,
        epoch: u64,
        members: &[MemberId],
        config: EngineConfig,
        store: St,
    ) -> Result<(Self, Output), ConversationError> {
        let mut engine = Self::assemble(conversation_id, own, epoch, members, config, store)?;
        engine.begin(Timestamp::ZERO);
        let phase = engine.phase();
        engine.emit(Event::PhaseChange(phase));
        // A fresh store: every key gets its first write.
        engine.dirty = Dirty::ALL;
        let out = engine.finish()?;
        Ok((engine, out))
    }

    /// Resume after a restart from what `store` holds. A store that was
    /// never written starts the engine as [`Self::join`] does. A snapshot
    /// written at another epoch than the group's is stale: scores and
    /// config are kept, everything else is dropped, and the output carries
    /// a sync request.
    pub fn restore(
        conversation_id: &str,
        own: MemberId,
        epoch: u64,
        members: &[MemberId],
        store: St,
    ) -> Result<(Self, Output), ConversationError> {
        let Some((config, snapshot_epoch)) = Self::load_config(&store)? else {
            return Self::join(
                conversation_id,
                own,
                epoch,
                members,
                EngineConfig::default(),
                store,
            );
        };
        let mut engine = Self::assemble(conversation_id, own, epoch, members, config, store)?;
        engine.begin(Timestamp::ZERO);
        if let Some(bytes) = Self::read_key(&engine.store, keys::SCORES)? {
            engine.restore_scores(&bytes)?;
        }

        if snapshot_epoch != epoch {
            warn!(
                conversation = conversation_id,
                snapshot_epoch,
                epoch,
                "restored snapshot is stale: dropping everything but scores and config"
            );
            engine.dirty = Dirty::ALL;
            engine.broadcast_sync_request();
        } else {
            if let Some(bytes) = Self::read_key(&engine.store, keys::STEWARD_LIST)? {
                engine.restore_steward_list(&bytes)?;
            }
            if let Some(bytes) = Self::read_key(&engine.store, keys::PROPOSALS)? {
                engine.restore_proposals(&bytes)?;
            }
            if let Some(bytes) = Self::read_key(&engine.store, keys::JOIN_EPOCHS)? {
                engine.restore_join_epochs(&bytes)?;
            }
            if let Some(bytes) = Self::read_key(&engine.store, keys::SKIPPED_STEWARDS)? {
                engine.restore_skipped_stewards(&bytes)?;
            }
            if let Some(bytes) = Self::read_key(&engine.store, keys::CONSENSUS)? {
                engine.restore_sessions(&bytes)?;
            }
        }

        let phase = engine.phase();
        engine.emit(Event::PhaseChange(phase));
        let out = engine.finish()?;
        Ok((engine, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ScoreEvent, ScoreOp,
        engine::{
            store::InMemoryStore,
            test_support::{id, member},
            types::{Outbound, Phase},
        },
        protos::de_mls::messages::v1::ConversationUpdateRequest,
    };

    /// A store that was never written starts the engine unsynced, exactly
    /// as `join` does, with no error.
    #[test]
    fn restore_without_a_snapshot_joins() {
        let (engine, out) = Engine::restore(
            "conv",
            id("alice"),
            0,
            &[id("alice"), id("bob")],
            InMemoryStore::default(),
        )
        .expect("restore over an empty store");
        assert!(!engine.is_synced());
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, Event::PhaseChange(Phase::Working)))
        );
    }

    /// A snapshot written at an epoch behind the group's is stale: scores
    /// carry over, the steward list and approved queue don't, and the
    /// output carries one sync request.
    #[test]
    fn restore_with_a_stale_snapshot_keeps_scores_and_asks_for_sync() {
        let members = [id("alice"), id("bob")];
        let (mut engine, _) = Engine::create(
            "conv",
            id("alice"),
            1,
            &members,
            EngineConfig::default(),
            InMemoryStore::default(),
        )
        .expect("create");
        engine.begin(Timestamp::ZERO);
        engine.apply_score_ops(&[ScoreOp {
            member_id: member("bob"),
            event: ScoreEvent::SuccessfulCommit,
        }]);
        engine
            .queues
            .insert_approved_proposal(7, ConversationUpdateRequest::remove_member(member("bob")));
        engine.dirty = Dirty::ALL;
        engine.flush().expect("flush");
        let bob_score = engine.scoring.score_for(&member("bob"));

        let store = engine.store().clone();
        let (restored, out) =
            Engine::restore("conv", id("alice"), 3, &members, store).expect("restore");

        assert_eq!(restored.scoring.score_for(&member("bob")), bob_score);
        assert!(restored.steward_list.current_list().is_none());
        assert_eq!(restored.queues.approved_proposals_count(), 0);
        assert_eq!(
            out.outbound
                .iter()
                .filter(|o| matches!(o, Outbound::Control(_)))
                .count(),
            1
        );
    }
}
