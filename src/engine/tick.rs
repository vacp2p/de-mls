//! Everything time-driven: the [`Engine::tick`] loop the router calls at each
//! wakeup and the freeze phase it advances.

use tracing::info;

use crate::{
    ConversationError,
    engine::{
        handle::Engine,
        store::EngineStore,
        types::{Event, Output, Phase, Timestamp},
    },
};

impl<St: EngineStore> Engine<St> {
    /// Run every deadline that has passed by `now`: consensus ticks, the
    /// freeze phase, the backup-takeover windows, and the inactivity freeze.
    ///
    /// Every step runs even when an earlier one failed, so there is no single
    /// error to return; each failure arrives as [`Event::Error`] naming its
    /// step.
    pub fn tick(&mut self, now: Timestamp) -> Result<Output, ConversationError> {
        self.begin(now);
        self.tick_deadlines();

        let result = self.advance_freezing();
        self.report_step("advance_freezing", result);

        let result = self.drive_sync_resend();
        self.report_step("drive_sync_resend", result);

        let result = self.drive_sync_request();
        self.report_step("drive_sync_request", result);

        let result = self.start_freeze_on_inactivity();
        self.report_step("start_freeze_on_inactivity", result);

        self.finish()
    }

    /// Report a failed step and carry on.
    fn report_step(&mut self, operation: &str, result: Result<(), ConversationError>) {
        if let Err(e) = result {
            self.report_failure(operation, &e);
        }
    }

    /// Drive the freeze phase. While `Freezing`, report progress as candidates
    /// arrive; the round closes only once the freeze window elapses, so the
    /// group has seen the epoch steward's commit before it merges.
    fn advance_freezing(&mut self) -> Result<(), ConversationError> {
        if self.phase != Phase::Freezing {
            self.timing.last_commit_round_progress = None;
            return Ok(());
        }

        let (received, expected) = self.commit_candidate_count();
        if !self.is_freeze_window_elapsed() {
            if self.timing.last_commit_round_progress != Some((received, expected)) {
                self.timing.last_commit_round_progress = Some((received, expected));
                self.emit(Event::CommitRoundProgress { received, expected });
            }
            return Ok(());
        }
        self.timing.last_commit_round_progress = None;

        let selection = self.start_selection();
        self.emit_phase(Some(selection));

        // The round completes in `commit_applied`, once the router reports
        // what the merge did to the group.
        self.close_round()
    }

    /// Steward-inactivity freeze entry: once the inactivity window passes with
    /// approved work pending, open the commit round. Held while an election is
    /// in flight — committing on a known-stale list only wastes a round.
    fn start_freeze_on_inactivity(&mut self) -> Result<(), ConversationError> {
        if self.queues.has_election_in_flight() {
            return Ok(());
        }
        let proposal_count = self.queues.approved_proposals_count();
        let inactivity = self.config.commit_batch_window;
        let Some(event) = self.check_steward_inactivity(proposal_count, inactivity) else {
            return Ok(());
        };
        info!(
            conversation = %self.conversation_id,
            approved = proposal_count,
            "steward inactivity transition"
        );
        self.on_freeze_entered(event)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        engine::{
            config::EngineConfig,
            store::InMemoryStore,
            types::{Action, CommitHash, Decision, MemberId, Phase, StagedFacts},
        },
        protos::de_mls::messages::v1::{ConversationUpdateRequest, MemberInvite},
    };

    fn at(secs: u64) -> Timestamp {
        Timestamp::from_duration_since_epoch(Duration::from_secs(secs))
    }

    fn id(name: &str) -> MemberId {
        MemberId::from(name.as_bytes())
    }

    /// A one-member conversation: the sole member is the epoch steward at
    /// every epoch.
    fn engine() -> Engine<InMemoryStore> {
        Engine::<InMemoryStore>::create(
            "conv",
            id("alice"),
            0,
            &[id("alice")],
            EngineConfig::default(),
            InMemoryStore::default(),
        )
        .expect("create")
        .0
    }

    fn invite(joiner: &str) -> ConversationUpdateRequest {
        ConversationUpdateRequest::member_invite(MemberInvite {
            key_package_bytes: format!("kp:{joiner}").into_bytes(),
            member_id: joiner.as_bytes().to_vec(),
        })
    }

    /// The epoch steward's candidate sits in the round until the freeze
    /// window elapses; it merges only then, not the instant it arrives.
    #[test]
    fn the_epoch_steward_candidate_merges_only_after_the_freeze_window() {
        let mut e = engine();
        e.queues.insert_approved_proposal(7, invite("bob"));

        e.begin(at(0));
        let event = e.start_freezing().expect("enters freezing");
        e.on_freeze_entered(event).unwrap();
        let hash = CommitHash::of(b"commit");
        let own = e.own.clone();
        e.handle_candidate(
            at(0),
            hash,
            StagedFacts {
                sender: MemberId::from(own.as_slice()),
                epoch: e.epoch,
                actions: vec![Action::Add {
                    member: id("bob"),
                    key_package: b"kp:bob".to_vec(),
                }],
                proposal_count: 1,
                self_removed: false,
            },
        )
        .unwrap();

        let before_window = e.tick(at(0)).unwrap();
        assert!(
            !before_window
                .decisions
                .iter()
                .any(|d| matches!(d, Decision::Merge { .. })),
            "no merge before the freeze window elapses"
        );
        assert_eq!(e.phase(), Phase::Freezing);

        let window = e.config.freeze_duration;
        let after_window = e.tick(at(0) + window).unwrap();
        assert!(
            after_window
                .decisions
                .iter()
                .any(|d| matches!(d, Decision::Merge { hash: h } if *h == hash)),
            "merges once the freeze window elapses"
        );
    }
}
