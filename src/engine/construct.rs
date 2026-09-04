//! Building an [`Engine`] from the facts the router holds: the conversation
//! id, this member, and the group's epoch and member set.

use crate::{
    ConversationError,
    engine::{
        config::EngineConfig,
        handle::Engine,
        store::EngineStore,
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
        let out = engine.finish();
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
        let out = engine.finish();
        Ok((engine, out))
    }

    /// Resume after a restart from what `store` holds. If the group's
    /// `epoch` is ahead of the snapshot, the engine keeps scores and meta,
    /// drops the rest, and asks for a sync.
    pub fn restore(
        conversation_id: &str,
        own: MemberId,
        epoch: u64,
        members: &[MemberId],
        store: St,
    ) -> Result<(Self, Output), ConversationError> {
        let config = EngineConfig::default();
        Self::join(conversation_id, own, epoch, members, config, store)
    }
}
