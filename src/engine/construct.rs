//! Building an [`Engine`] from the [`GroupView`] the router holds: the
//! conversation id, this member, and the group's epoch and member set.

use tracing::warn;

use crate::{
    ConversationError,
    engine::{
        EngineStore,
        config::EngineConfig,
        handle::Engine,
        store::{Dirty, keys},
        types::{Event, Output, Timestamp},
    },
    traits::{GroupView, Publish},
};

impl<St: EngineStore> Engine<St> {
    /// Creates the engine for a group this member founded.
    ///
    /// Call it once, right after creating the MLS group. `group` is the
    /// group as it stands: this member alone, or the founding members if
    /// they were seated in the same commit. Every current member becomes a
    /// steward from the current epoch, so the engine starts with a steward
    /// list and in `Working`; nothing is published.
    ///
    /// `now` is the caller's clock; every later call must pass a `now` that
    /// is not earlier. `store` must be empty; the engine writes every key
    /// before returning. Returns the engine and the [`Output`] of this
    /// call.
    ///
    /// # Errors
    ///
    /// [`ConversationError::EmptyMembersList`] if `group.members()` does
    /// not contain `group.own_id()`; a store or config error otherwise.
    pub fn create(
        now: Timestamp,
        group: &impl GroupView,
        config: EngineConfig,
        store: St,
        publisher: &mut impl Publish,
    ) -> Result<(Self, Output), ConversationError> {
        if !group.members().contains(&group.own_id()) {
            return Err(ConversationError::EmptyMembersList);
        }
        let mut engine = Self::assemble(group, config, store)?;
        engine.begin(now);
        for member in engine.members.clone() {
            engine.scoring.add_member(&member);
        }
        let settled = engine.members.clone();
        let sn = engine
            .steward_list
            .config()
            .compute_list_size(settled.len());
        engine
            .steward_list
            .install_list(engine.epoch, &settled, sn, 0)?;
        let phase = engine.phase();
        engine.emit(Event::PhaseChange(phase));
        // A fresh store: every key gets its first write.
        engine.dirty = Dirty::ALL;
        let out = engine.finish(publisher)?;
        Ok((engine, out))
    }

    /// Opens the engine for a group this member did not found.
    ///
    /// Call it after loading the MLS group, on the first start after being
    /// added and on every restart; `store` tells the two apart.
    ///
    /// - A store never written: this member was added to the group. The
    ///   engine starts in `Syncing`, publishes a request for the
    ///   conversation state, and adopts the answer when it arrives.
    /// - A store with a snapshot: a restart. The engine resumes from the
    ///   snapshot under the config stored with it; `config` is ignored. If
    ///   the snapshot was taken at another epoch than the group's, only
    ///   scores and config are kept and the engine starts as if added.
    ///
    /// `now` is the caller's clock; every later call must pass a `now` that
    /// is not earlier. Returns the engine and the [`Output`] of this call.
    ///
    /// # Errors
    ///
    /// A store that cannot be read, or a snapshot that does not decode.
    pub fn open(
        now: Timestamp,
        group: &impl GroupView,
        config: EngineConfig,
        store: St,
        publisher: &mut impl Publish,
    ) -> Result<(Self, Output), ConversationError> {
        match Self::load_config(&store)? {
            None => Self::added(now, group, config, store, publisher),
            Some((config, snapshot_epoch)) => {
                Self::restored(now, group, config, snapshot_epoch, store, publisher)
            }
        }
    }

    /// [`Self::open`] over a store never written: a member added to the
    /// group, with no steward list until a sync is adopted.
    fn added(
        now: Timestamp,
        group: &impl GroupView,
        config: EngineConfig,
        store: St,
        publisher: &mut impl Publish,
    ) -> Result<(Self, Output), ConversationError> {
        let mut engine = Self::assemble(group, config, store)?;
        engine
            .queues
            .record_join_epoch(engine.own.clone(), engine.epoch);
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

    /// [`Self::open`] over a written store: a restart, resumed from the
    /// snapshot taken at `snapshot_epoch` under the stored `config`.
    fn restored(
        now: Timestamp,
        group: &impl GroupView,
        config: EngineConfig,
        snapshot_epoch: u64,
        store: St,
        publisher: &mut impl Publish,
    ) -> Result<(Self, Output), ConversationError> {
        let mut engine = Self::assemble(group, config, store)?;
        engine.begin(now);
        if let Some(bytes) = Self::read_key(&engine.store, keys::SCORES)? {
            engine.restore_scores(&bytes)?;
        }

        if snapshot_epoch != engine.epoch {
            warn!(
                conversation = group.conversation_id(),
                snapshot_epoch,
                epoch = engine.epoch,
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
            test_support::{group, id, member, view},
            types::Phase,
        },
        protos::de_mls::messages::v1::ConversationUpdateRequest,
    };

    fn at(secs: u64) -> Timestamp {
        Timestamp::from_duration_since_epoch(Duration::from_secs(secs))
    }

    /// A store that was never written starts the engine unsynced, exactly
    /// as an added member, with no error.
    #[test]
    fn open_without_a_snapshot_starts_as_added() {
        let mut publisher = InMemoryPublisher::default();
        let (engine, out) = Engine::open(
            Timestamp::ZERO,
            &group("alice", 0, &["alice", "bob"]),
            EngineConfig::default(),
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
    fn open_with_a_stale_snapshot_keeps_scores_and_asks_for_sync() {
        let mut publisher = InMemoryPublisher::default();
        let members = [id("alice"), id("bob")];
        let (mut engine, _) = Engine::create(
            Timestamp::ZERO,
            &view(id("alice"), 1, &members),
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
        let (restored, out) = Engine::open(
            Timestamp::ZERO,
            &view(id("alice"), 3, &members),
            EngineConfig::default(),
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
        let (mut engine, out) = Engine::open(
            at(1_000),
            &group("bob", 1, &["alice", "bob"]),
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
    fn open_with_an_exhausted_list_files_the_election() {
        let mut publisher = InMemoryPublisher::default();
        let members = [id("alice"), id("bob"), id("carol"), id("dave")];
        let (mut engine, _) = Engine::create(
            at(0),
            &view(id("alice"), 2, &members),
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
        let (restored, out) = Engine::open(
            at(100),
            &view(id("alice"), 4, &members),
            EngineConfig::default(),
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

    /// A view whose member set leaves out this member cannot found a
    /// conversation.
    #[test]
    fn create_without_own_id_among_members_is_refused() {
        let mut publisher = InMemoryPublisher::default();
        let result = Engine::create(
            Timestamp::ZERO,
            &group("alice", 1, &["bob", "carol"]),
            EngineConfig::default(),
            InMemoryStore::default(),
            &mut publisher,
        );
        assert!(matches!(result, Err(ConversationError::EmptyMembersList)));
    }
}
