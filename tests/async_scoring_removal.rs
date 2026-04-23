//! Async integration tests for the peer scoring → steward-triggered removal pipeline.
//!
//! Exercises the full `User` layer with a shared `DefaultConsensusService`
//! and mock handlers. Packets are manually relayed between users.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use prost::Message;

use hashgraph_like_consensus::service::DefaultConsensusService;

use de_mls::app::{GroupConfig, GroupState, StateChangeHandler, User};
use de_mls::core::{CallbackError, DefaultProvider, GroupEventHandler, ProtocolConfig};
use de_mls::ds::{InboundPacket, OutboundPacket};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, GroupUpdateRequest, ViolationEvidence, ViolationType, app_message,
    group_update_request,
};

// ─────────────────────────── Mocks ───────────────────────────

#[derive(Clone)]
struct H {
    packets: Arc<Mutex<Vec<OutboundPacket>>>,
    app_msgs: Arc<Mutex<Vec<AppMessage>>>,
}

impl H {
    fn new() -> Self {
        Self {
            packets: Arc::new(Mutex::new(Vec::new())),
            app_msgs: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn drain_packets(&self) -> Vec<OutboundPacket> {
        std::mem::take(&mut *self.packets.lock().unwrap())
    }
    fn drain_app_msgs(&self) -> Vec<AppMessage> {
        std::mem::take(&mut *self.app_msgs.lock().unwrap())
    }
    fn last_vote_pid(&self) -> Option<u32> {
        self.app_msgs
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|msg| match &msg.payload {
                Some(app_message::Payload::VotePayload(vp)) => Some(vp.proposal_id),
                _ => None,
            })
    }
}

#[async_trait]
impl GroupEventHandler for H {
    async fn on_outbound(&self, _: &str, p: OutboundPacket) -> Result<String, CallbackError> {
        self.packets.lock().unwrap().push(p);
        Ok("ok".into())
    }
    async fn on_app_message(&self, _: &str, m: AppMessage) -> Result<(), CallbackError> {
        self.app_msgs.lock().unwrap().push(m);
        Ok(())
    }
    async fn on_leave_group(&self, _: &str) -> Result<(), CallbackError> {
        Ok(())
    }
    async fn on_joined_group(&self, _: &str) -> Result<(), CallbackError> {
        Ok(())
    }
    async fn on_error(&self, _: &str, _: &str, _: &str) {}
}

#[derive(Clone)]
struct SH;
#[async_trait]
impl StateChangeHandler for SH {
    async fn on_state_changed(&self, _: &str, _: GroupState) {}
}

// ─────────────────────────── Helpers ───────────────────────────

const ALICE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE_KEY: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

type TU = User<DefaultProvider, H, SH>;

fn make(key: &str, cs: Arc<DefaultConsensusService>, cfg: GroupConfig) -> (TU, H) {
    let h = H::new();
    let u =
        User::with_private_key_and_config(key, cs, Arc::new(h.clone()), Arc::new(SH), cfg).unwrap();
    (u, h)
}

fn to_in(p: &OutboundPacket) -> InboundPacket {
    InboundPacket::new(
        p.payload.clone(),
        &p.subtopic,
        &p.group_id,
        p.app_id.clone(),
        0,
    )
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Complete join flow: KP → vote → freeze → commit → welcome.
async fn do_join(
    steward: &mut TU,
    steward_h: &H,
    joiner: &mut TU,
    joiner_h: &H,
    others: &mut [(&mut TU, &H)],
    cs: &DefaultConsensusService,
    group: &str,
) {
    joiner.send_kp_message(group).await.unwrap();

    // Deliver KP to steward → triggers initiate_proposal (tokio::spawn)
    for p in joiner_h.drain_packets() {
        let _ = steward.process_inbound_packet(to_in(&p)).await;
    }
    settle().await;

    let pid = steward_h
        .last_vote_pid()
        .expect("should have invite VotePayload");
    // Steward auto-voted YES on creation (auto_vote=true). Don't vote again.

    // Relay proposal to others so they can vote
    for p in steward_h.drain_packets() {
        for (u, _) in others.iter_mut() {
            let _ = u.process_inbound_packet(to_in(&p)).await;
        }
    }
    settle().await;

    // Others vote YES
    for (u, h) in others.iter_mut() {
        let _ = u.process_user_vote(group, pid, true).await;
        for p in h.drain_packets() {
            let _ = steward.process_inbound_packet(to_in(&p)).await;
        }
    }
    settle().await;

    // Dispatch ConsensusReached to all EXISTING members only. The joiner isn't
    // subscribed to the group's consensus scope yet, so in production they
    // never see this event. Dispatching it to the joiner here would pollute
    // their local approved queue with the invite-self proposal (never cleared
    // because the joiner receives a welcome, not a commit, to enter the group).
    {
        use hashgraph_like_consensus::storage::ConsensusStorage;
        let scope = group.to_string();
        if let Ok(payload) = cs
            .storage()
            .get_proposal(&scope, pid)
            .await
            .map(|p| p.payload)
        {
            let ev = hashgraph_like_consensus::types::ConsensusEvent::ConsensusReached {
                proposal_id: pid,
                result: true,
                timestamp: 0,
            };
            let mut all: Vec<&mut TU> = vec![steward];
            for (u, _) in others.iter_mut() {
                all.push(u);
            }
            for u in all.iter_mut() {
                let _ = u.apply_consensus_outcome(group, ev.clone()).await;
            }
            let _ = payload;
        }
    }

    // `check_steward_inactivity` self-starts the timer on the first call
    // that sees approved work. Call once to kick it off, then wait for
    // epoch_duration, then call again to actually trigger the freeze
    // transition.
    steward.check_member_freeze(group).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    steward.check_member_freeze(group).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    steward.poll_freeze_status(group).await.unwrap();

    // Deliver commit candidate + welcome packets from the steward.
    // Others that receive the candidate enter Freezing; they need their own
    // poll_freeze_status to select and apply the commit.
    for p in steward_h.drain_packets() {
        let _ = joiner.process_inbound_packet(to_in(&p)).await;
        for (u, _) in others.iter_mut() {
            let _ = u.process_inbound_packet(to_in(&p)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    for (u, _) in others.iter_mut() {
        let _ = u.poll_freeze_status(group).await;
    }
}

/// Submit ECP from `submitter`, have everyone vote YES, apply consensus result directly.
async fn approve_ecp(
    submitter: &mut TU,
    submitter_h: &H,
    request: GroupUpdateRequest,
    group: &str,
    voters: &mut [(&mut TU, &H)],
    cs: &DefaultConsensusService,
) {
    submitter
        .initiate_proposal(group.to_string(), request)
        .await
        .unwrap();
    settle().await;

    let pid = submitter_h
        .last_vote_pid()
        .expect("should have ECP VotePayload");

    // Submitter auto-voted YES on creation. Relay proposal to voters.
    for p in submitter_h.drain_packets() {
        for (u, _) in voters.iter_mut() {
            let _ = u.process_inbound_packet(to_in(&p)).await;
        }
    }
    settle().await;

    // Voters vote YES
    for (u, h) in voters.iter_mut() {
        let _ = u.process_user_vote(group, pid, true).await;
        for p in h.drain_packets() {
            let _ = submitter.process_inbound_packet(to_in(&p)).await;
        }
    }
    settle().await;

    // Directly dispatch the consensus event to all users
    {
        use hashgraph_like_consensus::storage::ConsensusStorage;
        let scope = group.to_string();
        if (cs
            .storage()
            .get_proposal(&scope, pid)
            .await
            .map(|p| p.payload))
        .is_ok()
        {
            let ev = hashgraph_like_consensus::types::ConsensusEvent::ConsensusReached {
                proposal_id: pid,
                result: true,
                timestamp: 0,
            };
            let mut all: Vec<&mut TU> = vec![submitter];
            for (u, _) in voters.iter_mut() {
                all.push(u);
            }
            for u in all.iter_mut() {
                let _ = u.apply_consensus_outcome(group, ev.clone()).await;
            }
        }
    }
    settle().await;
}

// ─────────────────────────── Tests ───────────────────────────

/// Full pipeline: penalties → threshold → auto SCORE_BELOW_THRESHOLD ECP → RemoveMember.
#[tokio::test]
async fn test_user_layer_score_removal_pipeline() {
    let group = "score-removal";
    let cfg = GroupConfig {
        epoch_duration: Duration::from_millis(50),
        freeze_duration: Duration::from_millis(10),
        protocol: ProtocolConfig::new(1, 5).unwrap(),
        ..GroupConfig::default()
    };
    let cs = Arc::new(DefaultConsensusService::new_with_max_sessions(100));

    let (mut alice, ah) = make(ALICE_KEY, cs.clone(), cfg.clone());
    let (mut bob, bh) = make(BOB_KEY, cs.clone(), cfg.clone());
    let (mut charlie, ch) = make(CHARLIE_KEY, cs.clone(), cfg.clone());

    // Step 1: Create group + join Bob + Charlie
    alice.create_group(group, true).await.unwrap();

    bob.create_group(group, false).await.unwrap();
    do_join(&mut alice, &ah, &mut bob, &bh, &mut [], &cs, group).await;
    assert_eq!(
        bob.get_group_state(group).await.unwrap(),
        GroupState::Working
    );

    // After Bob joins, the epoch has advanced past Alice's initial single-steward
    // list (start_epoch=0, len=1). Simulate the steward election completing
    // so someone is the live epoch steward before Charlie joins.
    alice.regenerate_steward_list(group).await.unwrap();
    bob.regenerate_steward_list(group).await.unwrap();

    // Deterministic generation decides whether Alice or Bob ends up at list[0];
    // both are valid stewards from the protocol's perspective. Query the live
    // epoch steward via the public role API and route Charlie's join through
    // them. Avoids baking a specific hash ordering into the test.
    use de_mls::app::MemberRole;
    let alice_bytes = de_mls::mls_crypto::parse_wallet_to_bytes(&alice.identity_string()).unwrap();
    let alice_is_epoch_steward = alice
        .get_member_roles(group)
        .await
        .unwrap()
        .iter()
        .any(|(id, role)| *role == MemberRole::EpochSteward && *id == alice_bytes);

    charlie.create_group(group, false).await.unwrap();
    if alice_is_epoch_steward {
        do_join(
            &mut alice,
            &ah,
            &mut charlie,
            &ch,
            &mut [(&mut bob, &bh)],
            &cs,
            group,
        )
        .await;
    } else {
        do_join(
            &mut bob,
            &bh,
            &mut charlie,
            &ch,
            &mut [(&mut alice, &ah)],
            &cs,
            group,
        )
        .await;
    }
    assert_eq!(
        charlie.get_group_state(group).await.unwrap(),
        GroupState::Working
    );

    assert_eq!(alice.get_group_members(group).await.unwrap().len(), 3);

    let bob_bytes = de_mls::mls_crypto::parse_wallet_to_bytes(&bob.identity_string()).unwrap();
    let alice_bytes = de_mls::mls_crypto::parse_wallet_to_bytes(&alice.identity_string()).unwrap();

    // Baseline after setup — setup commits contributed SuccessfulCommit and
    // HonestCommitAttempt rewards, so scores are >= 100.
    let baseline_alice = alice.get_member_score(group, &alice_bytes).unwrap();
    let baseline_bob = alice.get_member_score(group, &bob_bytes).unwrap();

    // Drive Bob below the removal threshold (0). Each BrokenCommit ECP costs
    // him 50; the creator (Alice) gains 20 per accepted ECP.
    let ecps_to_remove = ((baseline_bob + 49) / 50) as usize;

    for i in 0..ecps_to_remove {
        let ecp = ViolationEvidence::broken_commit(bob_bytes.clone(), 0, Vec::<u8>::new())
            .with_creator(alice_bytes.clone())
            .into_update_request()
            .unwrap();

        approve_ecp(
            &mut alice,
            &ah,
            ecp,
            group,
            &mut [(&mut bob, &bh), (&mut charlie, &ch)],
            &cs,
        )
        .await;

        let expected_bob = baseline_bob - 50 * (i as i64 + 1);
        assert_eq!(
            alice.get_member_score(group, &bob_bytes),
            Some(expected_bob)
        );
        assert_eq!(
            alice.get_member_score(group, &alice_bytes),
            Some(baseline_alice + 20 * (i as i64 + 1))
        );
    }

    // Step 5: Steward should have auto-created SCORE_BELOW_THRESHOLD ECP.
    // check_and_initiate_score_removals runs inside apply_consensus_outcome.
    settle().await;

    // Check that the SCORE_BELOW_THRESHOLD ECP was created
    let removal_created = ah.drain_app_msgs().iter().any(|msg| {
        if let Some(app_message::Payload::VotePayload(vp)) = &msg.payload {
            if let Ok(req) = GroupUpdateRequest::decode(vp.payload.as_slice()) {
                if let Some(group_update_request::Payload::EmergencyCriteria(ec)) = &req.payload {
                    if let Some(ev) = &ec.evidence {
                        return ViolationType::try_from(ev.violation_type)
                            == Ok(ViolationType::ScoreBelowThreshold)
                            && ev.target_member_id == bob_bytes;
                    }
                }
            }
        }
        false
    });

    assert!(
        removal_created,
        "Steward should have auto-created a SCORE_BELOW_THRESHOLD ECP for Bob"
    );

    // The core transformation (ECP YES → RemoveMember in approved queue) is
    // thoroughly tested in steward_triggered_removal.rs. Here we verified the
    // full User-layer pipeline: penalties → threshold detection → auto ECP creation.
}

/// Accepted ECP updates scores consistently on all nodes.
#[tokio::test]
async fn test_ecp_scores_applied_on_all_nodes() {
    let group = "score-sync";
    let cfg = GroupConfig {
        epoch_duration: Duration::from_millis(50),
        freeze_duration: Duration::from_millis(10),
        protocol: ProtocolConfig::new(1, 5).unwrap(),
        ..GroupConfig::default()
    };
    let cs = Arc::new(DefaultConsensusService::new_with_max_sessions(100));

    let (mut alice, ah) = make(ALICE_KEY, cs.clone(), cfg.clone());
    let (mut bob, bh) = make(BOB_KEY, cs.clone(), cfg.clone());

    alice.create_group(group, true).await.unwrap();
    bob.create_group(group, false).await.unwrap();
    do_join(&mut alice, &ah, &mut bob, &bh, &mut [], &cs, group).await;
    assert_eq!(
        bob.get_group_state(group).await.unwrap(),
        GroupState::Working
    );

    let bob_bytes = de_mls::mls_crypto::parse_wallet_to_bytes(&bob.identity_string()).unwrap();
    let alice_bytes = de_mls::mls_crypto::parse_wallet_to_bytes(&alice.identity_string()).unwrap();

    // Baseline after setup — the join commit rewards the epoch steward.
    let baseline_alice = alice.get_member_score(group, &alice_bytes).unwrap();
    let baseline_bob = alice.get_member_score(group, &bob_bytes).unwrap();

    let ecp = ViolationEvidence::broken_mls_proposal(bob_bytes.clone(), 0, Vec::<u8>::new())
        .with_creator(alice_bytes.clone())
        .into_update_request()
        .unwrap();

    approve_ecp(&mut alice, &ah, ecp, group, &mut [(&mut bob, &bh)], &cs).await;

    // Both nodes apply identical deltas: target -30, creator +20.
    assert_eq!(
        alice.get_member_score(group, &bob_bytes),
        Some(baseline_bob - 30)
    );
    assert_eq!(
        bob.get_member_score(group, &bob_bytes),
        Some(baseline_bob - 30)
    );
    assert_eq!(
        alice.get_member_score(group, &alice_bytes),
        Some(baseline_alice + 20)
    );
    assert_eq!(
        bob.get_member_score(group, &alice_bytes),
        Some(baseline_alice + 20)
    );
}
