use std::collections::HashMap;

use de_mls::app::{FixedScoringProvider, InMemoryPeerScoreStorage, PeerScoringService};
use de_mls::core::{ScoreEvent, ScoringConfig};

const GROUP: &str = "test-group";

fn default_config() -> ScoringConfig {
    ScoringConfig {
        default_score: 100,
        removal_threshold: 0,
    }
}

fn default_deltas() -> HashMap<ScoreEvent, i64> {
    HashMap::from([
        (ScoreEvent::BrokenCommit, -50),
        (ScoreEvent::BrokenMlsProposal, -30),
        (ScoreEvent::CensorshipInactivity, -40),
        (ScoreEvent::EmergencyYesCreator, 20),
        (ScoreEvent::EmergencyNoCreator, -50),
        (ScoreEvent::SuccessfulCommit, 10),
        (ScoreEvent::NonFinalizedProposalCommit, -30),
    ])
}

fn make_service() -> PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider> {
    PeerScoringService::new(
        InMemoryPeerScoreStorage::new(),
        FixedScoringProvider::new(default_deltas()),
        default_config(),
    )
}

#[test]
fn test_add_member_gets_default_score() {
    let mut svc = make_service();
    let member = b"alice";

    svc.add_member(GROUP, member);

    assert_eq!(svc.score_for(GROUP, member), Some(100));
}

#[test]
fn test_unknown_member_returns_none() {
    let svc = make_service();

    assert_eq!(svc.score_for(GROUP, b"unknown"), None);
}

#[test]
fn test_remove_member() {
    let mut svc = make_service();
    let member = b"alice";

    svc.add_member(GROUP, member);
    svc.remove_member(GROUP, member);

    assert_eq!(svc.score_for(GROUP, member), None);
}

#[test]
fn test_apply_event_decreases_score() {
    let mut svc = make_service();
    let member = b"alice";
    svc.add_member(GROUP, member);

    let new_score = svc.apply_event(GROUP, member, ScoreEvent::EmergencyNoCreator);

    assert_eq!(new_score, Some(50)); // 100 + (-50) = 50
    assert_eq!(svc.score_for(GROUP, member), Some(50));
}

#[test]
fn test_apply_event_increases_score() {
    let mut svc = make_service();
    let member = b"alice";
    svc.add_member(GROUP, member);

    let new_score = svc.apply_event(GROUP, member, ScoreEvent::SuccessfulCommit);

    assert_eq!(new_score, Some(110)); // 100 + 10 = 110
}

#[test]
fn test_apply_event_unknown_member_returns_none() {
    let mut svc = make_service();

    let result = svc.apply_event(GROUP, b"unknown", ScoreEvent::EmergencyNoCreator);

    assert_eq!(result, None);
}

#[test]
fn test_multiple_events_accumulate() {
    let mut svc = make_service();
    let member = b"alice";
    svc.add_member(GROUP, member);

    svc.apply_event(GROUP, member, ScoreEvent::EmergencyNoCreator); // 100 - 50 = 50
    svc.apply_event(GROUP, member, ScoreEvent::NonFinalizedProposalCommit); // 50 - 30 = 20
    svc.apply_event(GROUP, member, ScoreEvent::SuccessfulCommit); // 20 + 10 = 30

    assert_eq!(svc.score_for(GROUP, member), Some(30));
}

#[test]
fn test_members_below_threshold() {
    let mut svc = make_service();
    svc.add_member(GROUP, b"alice");
    svc.add_member(GROUP, b"bob");
    svc.add_member(GROUP, b"charlie");

    // Drop alice to threshold: 100 - 50 - 50 = 0
    svc.apply_event(GROUP, b"alice", ScoreEvent::EmergencyNoCreator);
    svc.apply_event(GROUP, b"alice", ScoreEvent::BrokenCommit);

    // Bob stays at 100
    // Drop charlie to exactly 0 (threshold): 100 - 50 - 50 = 0
    svc.apply_event(GROUP, b"charlie", ScoreEvent::EmergencyNoCreator);
    svc.apply_event(GROUP, b"charlie", ScoreEvent::EmergencyNoCreator);

    let below = svc.members_below_threshold(GROUP);
    assert_eq!(below.len(), 2);
    assert!(below.contains(&b"alice".to_vec()));
    assert!(below.contains(&b"charlie".to_vec()));
    assert!(!below.contains(&b"bob".to_vec()));
}

#[test]
fn test_is_below_threshold() {
    let mut svc = make_service();
    svc.add_member(GROUP, b"alice");

    assert!(!svc.is_below_threshold(GROUP, b"alice")); // 100 > 0

    // Drop to 0 (at threshold)
    svc.apply_event(GROUP, b"alice", ScoreEvent::EmergencyNoCreator);
    svc.apply_event(GROUP, b"alice", ScoreEvent::BrokenCommit);

    assert!(svc.is_below_threshold(GROUP, b"alice")); // 0 <= 0
}

#[test]
fn test_is_below_threshold_unknown_member() {
    let svc = make_service();

    assert!(!svc.is_below_threshold(GROUP, b"unknown"));
}

#[test]
fn test_score_saturates_no_overflow() {
    let mut svc = PeerScoringService::new(
        InMemoryPeerScoreStorage::new(),
        FixedScoringProvider::new(HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)])),
        ScoringConfig {
            default_score: i64::MAX,
            removal_threshold: 0,
        },
    );
    svc.add_member(GROUP, b"alice");

    let new_score = svc.apply_event(GROUP, b"alice", ScoreEvent::SuccessfulCommit);

    assert_eq!(new_score, Some(i64::MAX)); // saturating_add prevents overflow
}

#[test]
fn test_unknown_event_in_provider_returns_zero_delta() {
    // Create a provider with only one event configured
    let mut svc = PeerScoringService::new(
        InMemoryPeerScoreStorage::new(),
        FixedScoringProvider::new(HashMap::from([(ScoreEvent::EmergencyNoCreator, -50)])),
        default_config(),
    );
    svc.add_member(GROUP, b"alice");

    // SuccessfulCommit is not in the deltas map → delta = 0
    let new_score = svc.apply_event(GROUP, b"alice", ScoreEvent::SuccessfulCommit);

    assert_eq!(new_score, Some(100)); // unchanged
}

#[test]
fn test_determinism_independent_instances() {
    // Two independent services with identical config produce identical scores.
    let events = vec![
        (b"alice".as_slice(), ScoreEvent::EmergencyNoCreator),
        (b"alice".as_slice(), ScoreEvent::SuccessfulCommit),
        (b"bob".as_slice(), ScoreEvent::BrokenCommit),
        (b"bob".as_slice(), ScoreEvent::SuccessfulCommit),
    ];

    let mut svc1 = make_service();
    let mut svc2 = make_service();

    for svc in [&mut svc1, &mut svc2] {
        svc.add_member(GROUP, b"alice");
        svc.add_member(GROUP, b"bob");
    }

    for (member, event) in &events {
        svc1.apply_event(GROUP, member, *event);
        svc2.apply_event(GROUP, member, *event);
    }

    assert_eq!(
        svc1.score_for(GROUP, b"alice"),
        svc2.score_for(GROUP, b"alice")
    );
    assert_eq!(svc1.score_for(GROUP, b"bob"), svc2.score_for(GROUP, b"bob"));
    assert_eq!(
        svc1.members_below_threshold(GROUP).len(),
        svc2.members_below_threshold(GROUP).len()
    );
}

#[test]
fn test_false_accusation_penalty() {
    let mut svc = make_service();
    svc.add_member(GROUP, b"accuser");
    svc.add_member(GROUP, b"target");

    // Emergency rejected → creator penalized, target unaffected
    svc.apply_event(GROUP, b"accuser", ScoreEvent::EmergencyNoCreator); // 100 - 50 = 50

    assert_eq!(svc.score_for(GROUP, b"accuser"), Some(50));
    assert_eq!(svc.score_for(GROUP, b"target"), Some(100)); // unchanged
}

#[test]
fn test_scores_isolated_between_groups() {
    let mut svc = make_service();
    let group_a = "group-a";
    let group_b = "group-b";
    let member = b"alice";

    svc.add_member(group_a, member);
    svc.add_member(group_b, member);

    // Penalize in group_a only
    svc.apply_event(group_a, member, ScoreEvent::EmergencyNoCreator); // 100 - 50 = 50

    assert_eq!(svc.score_for(group_a, member), Some(50));
    assert_eq!(svc.score_for(group_b, member), Some(100)); // unaffected
}

#[test]
fn test_members_below_threshold_only_returns_group_members() {
    let mut svc = make_service();
    let group_a = "group-a";
    let group_b = "group-b";

    svc.add_member(group_a, b"alice");
    svc.add_member(group_b, b"bob");

    // Drop both below threshold
    svc.apply_event(group_a, b"alice", ScoreEvent::EmergencyNoCreator);
    svc.apply_event(group_a, b"alice", ScoreEvent::BrokenCommit);
    svc.apply_event(group_b, b"bob", ScoreEvent::EmergencyNoCreator);
    svc.apply_event(group_b, b"bob", ScoreEvent::BrokenCommit);

    let below_a = svc.members_below_threshold(group_a);
    let below_b = svc.members_below_threshold(group_b);

    assert_eq!(below_a, vec![b"alice".to_vec()]);
    assert_eq!(below_b, vec![b"bob".to_vec()]);
}
