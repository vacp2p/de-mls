//! Building an [`Engine`] from the facts the router holds: the conversation
//! id, this member, and the group's epoch and member set.

use tracing::warn;

use crate::{
    ConversationError,
    engine::{
        EngineStore,
        config::EngineConfig,
        handle::Engine,
        store::{Dirty, keys},
        types::{Event, MemberId, Output, Timestamp},
    },
    traits::Publish,
};

impl<St: EngineStore> Engine<St> {
    /// Start a conversation this member created. `members` is the group's
    /// member set at `epoch`, `own` included. A single member is a plain
    /// creation; several are a founding group the router seated in one
    /// commit before calling, all of them settled stewards from `epoch`.
    /// `now` is the router's clock, the one every later call passes.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        now: Timestamp,
        conversation_id: &str,
        own: MemberId,
        epoch: u64,
        members: &[MemberId],
        config: EngineConfig,
        store: St,
        publisher: &mut impl Publish,
    ) -> Result<(Self, Output), ConversationError> {
        if !members.contains(&own) {
            return Err(ConversationError::EmptyMembersList);
        }
        let mut engine = Self::assemble(conversation_id, own, epoch, members, config, store)?;
        engine.begin(now);
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
        let out = engine.finish(publisher)?;
        Ok((engine, out))
    }

    /// Start a conversation this member joined from a welcome. The welcome
    /// carries nothing but the group state: the engine starts in `Syncing`,
    /// asks a steward for a `ConversationSync` in the returned `Output`, and
    /// adopts the answer. `now` is the router's clock, the one every later
    /// call passes.
    #[allow(clippy::too_many_arguments)]
    pub fn join(
        now: Timestamp,
        conversation_id: &str,
        own: MemberId,
        epoch: u64,
        members: &[MemberId],
        config: EngineConfig,
        store: St,
        publisher: &mut impl Publish,
    ) -> Result<(Self, Output), ConversationError> {
        let mut engine = Self::assemble(conversation_id, own, epoch, members, config, store)?;
        engine.queues.record_join_epoch(engine.own.clone(), epoch);
        engine.begin(now);
        // A joiner holds no steward list: ask for the sync in this very
        // `Output`.
        let syncing = engine.start_syncing();
        engine.emit_phase(Some(syncing));
        engine.broadcast_sync_request();
        // A fresh store: every key gets its first write.
        engine.dirty = Dirty::ALL;
        let out = engine.finish(publisher)?;
        Ok((engine, out))
    }

    /// Resume after a restart from what `store` holds. A store that was
    /// never written starts the engine as [`Self::join`] does. A snapshot
    /// written at another epoch than the group's is stale: scores and
    /// config are kept, everything else is dropped, and the output carries
    /// a sync request. `now` is the router's clock, the one every later
    /// call passes.
    pub fn restore(
        now: Timestamp,
        conversation_id: &str,
        own: MemberId,
        epoch: u64,
        members: &[MemberId],
        store: St,
        publisher: &mut impl Publish,
    ) -> Result<(Self, Output), ConversationError> {
        let Some((config, snapshot_epoch)) = Self::load_config(&store)? else {
            return Self::join(
                now,
                conversation_id,
                own,
                epoch,
                members,
                EngineConfig::default(),
                store,
                publisher,
            );
        };
        let mut engine = Self::assemble(conversation_id, own, epoch, members, config, store)?;
        engine.begin(now);
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
            let syncing = engine.start_syncing();
            engine.emit_phase(Some(syncing));
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
            // The phase follows the restored list: `Working` when it covers
            // the epoch, `Syncing` when the election it ran out into is still
            // to be filed or voted. No ask: nobody holds a newer list.
            engine.reconcile_list_and_phase();
        }

        let out = engine.finish(publisher)?;
        Ok((engine, out))
    }
}

#[cfg(test)]
mod tests {
    use crate::traits::InMemoryPublisher;
    use std::time::Duration;

    use super::*;
    use crate::{
        ScoreEvent, ScoreOp,
        engine::{
            InMemoryStore,
            test_support::{id, member},
            types::Phase,
        },
        protos::de_mls::messages::v1::ConversationUpdateRequest,
    };

    fn at(secs: u64) -> Timestamp {
        Timestamp::from_duration_since_epoch(Duration::from_secs(secs))
    }

    /// A store that was never written starts the engine unsynced, exactly
    /// as `join` does, with no error.
    #[test]
    fn restore_without_a_snapshot_joins() {
        let mut publisher = InMemoryPublisher::default();
        let (engine, out) = Engine::restore(
            Timestamp::ZERO,
            "conv",
            id("alice"),
            0,
            &[id("alice"), id("bob")],
            InMemoryStore::default(),
            &mut publisher,
        )
        .expect("restore over an empty store");
        assert!(!engine.is_synced());
        assert_eq!(engine.phase(), Phase::Syncing);
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, Event::PhaseChange(Phase::Syncing)))
        );
        assert_eq!(publisher.messages.len(), 1, "a joiner asks for a sync");
        assert_eq!(out.wakeup, None, "nothing is due after the ask");
    }

    /// A snapshot written at an epoch behind the group's is stale: scores
    /// carry over, the steward list and approved queue don't, and the
    /// output carries one sync request.
    #[test]
    fn restore_with_a_stale_snapshot_keeps_scores_and_asks_for_sync() {
        let mut publisher = InMemoryPublisher::default();
        let members = [id("alice"), id("bob")];
        let (mut engine, _) = Engine::create(
            Timestamp::ZERO,
            "conv",
            id("alice"),
            1,
            &members,
            EngineConfig::default(),
            InMemoryStore::default(),
            &mut publisher,
        )
        .expect("create");
        engine.begin(at(0));
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
        publisher.drain();
        let (restored, out) = Engine::restore(
            Timestamp::ZERO,
            "conv",
            id("alice"),
            3,
            &members,
            store,
            &mut publisher,
        )
        .expect("restore");

        assert_eq!(restored.scoring.score_for(&member("bob")), bob_score);
        assert!(restored.steward_list.current_list().is_none());
        assert_eq!(restored.queues.approved_proposals_count(), 0);
        assert_eq!(restored.phase(), Phase::Syncing);
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, Event::PhaseChange(Phase::Syncing)))
        );
        assert_eq!(publisher.messages.len(), 1);
        assert_eq!(out.wakeup, None, "nothing is due after the ask");
    }

    /// A joiner asks for a sync in the output of its join and does not ask
    /// again on its own.
    #[test]
    fn a_joiner_asks_for_a_sync_once_at_join() {
        let mut publisher = InMemoryPublisher::default();
        let (mut engine, out) = Engine::join(
            at(1_000),
            "conv",
            id("bob"),
            1,
            &[id("alice"), id("bob")],
            EngineConfig::default(),
            InMemoryStore::default(),
            &mut publisher,
        )
        .expect("join");
        let turns = engine.config().backup_takeover_window * 2;

        assert_eq!(engine.phase(), Phase::Syncing);
        assert_eq!(publisher.messages.len(), 1);
        assert_eq!(out.wakeup, None);

        publisher.drain();
        engine
            .tick(at(1_000) + turns * 2, &mut publisher)
            .expect("tick");
        assert!(publisher.messages.is_empty());
        assert_eq!(engine.phase(), Phase::Syncing);
    }

    /// A responsible proposer restarting into an exhausted list files the
    /// election from the restart, on the restart's clock.
    #[test]
    fn restore_with_an_exhausted_list_files_the_election() {
        let mut publisher = InMemoryPublisher::default();
        let members = [id("alice"), id("bob"), id("carol"), id("dave")];
        let (mut engine, _) = Engine::create(
            at(0),
            "conv",
            id("alice"),
            2,
            &members,
            EngineConfig::default(),
            InMemoryStore::default(),
            &mut publisher,
        )
        .expect("create");
        // The list installed at creation has `sn_max` (2) members and covers
        // epochs 2-3; epoch 4 runs it out.
        engine.epoch = 4;
        engine.dirty = Dirty::ALL;
        engine.flush().expect("flush");

        let store = engine.store().clone();
        publisher.drain();
        let (restored, out) = Engine::restore(
            at(100),
            "conv",
            id("alice"),
            4,
            &members,
            store,
            &mut publisher,
        )
        .expect("restore");

        assert_eq!(restored.phase(), Phase::Syncing);
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, Event::PhaseChange(Phase::Syncing)))
        );
        assert_eq!(
            publisher.messages.len(),
            1,
            "alice is the first eligible member of the exhausted list, so files the election"
        );
    }
}
