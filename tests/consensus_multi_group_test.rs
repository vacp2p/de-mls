use alloy::signers::local::PrivateKeySigner;
use de_mls::consensus::{compute_vote_hash, ConsensusEvent, ConsensusService};
use de_mls::protos::consensus::v1::Vote;
use de_mls::LocalSigner;
use prost::Message;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn test_basic_consensus_service() {
    // Create consensus service
    let consensus_service = ConsensusService::new();

    let group_name = "test_group";
    let expected_voters_count = 3;

    let signer = PrivateKeySigner::random();
    let proposal_owner_address = signer.address();
    let proposal_owner = proposal_owner_address.to_string().as_bytes().to_vec();

    // Create a proposal
    let proposal = consensus_service
        .create_proposal(
            group_name,
            "Test Proposal".to_string(),
            vec![],
            proposal_owner,
            expected_voters_count,
            300,
            true,
        )
        .await
        .expect("Failed to create proposal");

    let proposal = consensus_service
        .vote_on_proposal(group_name, proposal.proposal_id, true, signer)
        .await
        .expect("Failed to vote on proposal");

    // Verify proposal was created
    let active_proposals = consensus_service.get_active_proposals(group_name).await;
    assert_eq!(active_proposals.len(), 1);
    assert_eq!(active_proposals[0].proposal_id, proposal.proposal_id);

    // Verify group statistics
    let group_stats = consensus_service.get_group_stats(group_name).await;
    assert_eq!(group_stats.total_sessions, 1);
    assert_eq!(group_stats.active_sessions, 1);

    // Verify consensus threshold calculation
    // With 3 expected voters, we need 2n/3 = 2 votes for consensus
    // Initially we have 1 vote (steward), so we don't have sufficient votes
    assert!(
        !consensus_service
            .has_sufficient_votes(group_name, proposal.proposal_id)
            .await
    );

    let signer_2 = PrivateKeySigner::random();
    let proposal_owner_2 = signer_2.address_bytes();
    // Add 1 more vote (total 2 votes)
    let mut vote = Vote {
        vote_id: Uuid::new_v4().as_u128() as u32,
        vote_owner: proposal_owner_2,
        proposal_id: proposal.proposal_id,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Failed to get current time")
            .as_secs(),
        vote: true,
        parent_hash: Vec::new(),
        received_hash: proposal.votes[0].vote_hash.clone(), // Reference steward's vote hash
        vote_hash: Vec::new(),
        signature: Vec::new(),
    };

    // Compute vote hash
    vote.vote_hash = compute_vote_hash(&vote);
    let vote_bytes = vote.encode_to_vec();
    vote.signature = signer_2
        .local_sign_message(&vote_bytes)
        .await
        .expect("Failed to sign vote");

    consensus_service
        .process_incoming_vote(group_name, vote)
        .await
        .expect("Failed to process vote");

    // Now we should have sufficient votes (2 out of 3 expected voters)
    assert!(
        consensus_service
            .has_sufficient_votes(group_name, proposal.proposal_id)
            .await
    );
}

#[tokio::test]
async fn test_multi_group_consensus_service() {
    // Create consensus service with max 10 sessions per group
    let consensus_service = ConsensusService::new_with_max_sessions(10);

    // Test group 1
    let group1_name = "test_group_1";
    let group1_members_count = 3;
    let signer_1 = PrivateKeySigner::random();
    let proposal_owner_1 = signer_1.address_bytes();

    // Test group 2
    let group2_name = "test_group_2";
    let group2_members_count = 3;
    let signer_2 = PrivateKeySigner::random();
    let proposal_owner_2 = signer_2.address_bytes();

    // Create proposals for group 1
    let proposal_1 = consensus_service
        .create_proposal(
            group1_name,
            "Test Proposal".to_string(),
            vec![],
            proposal_owner_1,
            group1_members_count,
            300,
            true,
        )
        .await
        .expect("Failed to create proposal");

    let _proposal_1 = consensus_service
        .vote_on_proposal(group1_name, proposal_1.proposal_id, true, signer_1)
        .await
        .expect("Failed to vote on proposal");

    let proposal_2 = consensus_service
        .create_proposal(
            group2_name,
            "Test Proposal".to_string(),
            vec![],
            proposal_owner_2.clone(),
            group2_members_count,
            300,
            true,
        )
        .await
        .expect("Failed to create proposal");

    let _proposal_2 = consensus_service
        .vote_on_proposal(group2_name, proposal_2.proposal_id, true, signer_2.clone())
        .await
        .expect("Failed to vote on proposal");

    // Create proposal for group 2
    let proposal_3 = consensus_service
        .create_proposal(
            group2_name,
            "Test Proposal".to_string(),
            vec![],
            proposal_owner_2,
            group2_members_count,
            300,
            true,
        )
        .await
        .expect("Failed to create proposal");

    let _proposal_3 = consensus_service
        .vote_on_proposal(group2_name, proposal_3.proposal_id, true, signer_2)
        .await
        .expect("Failed to vote on proposal");

    // Verify proposals are created for both groups
    let group1_proposals = consensus_service.get_active_proposals(group1_name).await;
    let group2_proposals = consensus_service.get_active_proposals(group2_name).await;

    assert_eq!(group1_proposals.len(), 1);
    assert_eq!(group2_proposals.len(), 2);

    // Verify group statistics
    let group1_stats = consensus_service.get_group_stats(group1_name).await;
    let group2_stats = consensus_service.get_group_stats(group2_name).await;

    assert_eq!(group1_stats.total_sessions, 1);
    assert_eq!(group1_stats.active_sessions, 1);
    assert_eq!(group2_stats.total_sessions, 2);
    assert_eq!(group2_stats.active_sessions, 2);

    // Verify overall statistics
    let overall_stats = consensus_service.get_overall_stats().await;
    assert_eq!(overall_stats.total_sessions, 3);
    assert_eq!(overall_stats.active_sessions, 3);

    // Verify active groups
    let active_groups = consensus_service.get_active_groups().await;
    assert_eq!(active_groups.len(), 2);
    assert!(active_groups.contains(&group1_name.to_string()));
    assert!(active_groups.contains(&group2_name.to_string()));
}

#[tokio::test]
async fn test_consensus_threshold_calculation() {
    let consensus_service = ConsensusService::new();
    let mut consensus_events = consensus_service.subscribe_to_events();

    let group_name = "test_group_threshold";
    let expected_voters_count = 5;
    let signer = PrivateKeySigner::random();
    let proposal_owner = signer.address_bytes();

    // Create a proposal
    let proposal = consensus_service
        .create_proposal(
            group_name,
            "Test Proposal".to_string(),
            vec![],
            proposal_owner,
            expected_voters_count,
            300,
            true,
        )
        .await
        .expect("Failed to create proposal");

    let proposal = consensus_service
        .vote_on_proposal(group_name, proposal.proposal_id, true, signer)
        .await
        .expect("Failed to vote on proposal");

    // With 5 expected voters, we need 2n/3 = 3.33... -> 4 votes for consensus
    // Initially we have 1 vote (steward), so we don't have sufficient votes
    assert!(
        !consensus_service
            .has_sufficient_votes(group_name, proposal.proposal_id)
            .await
    );

    for _ in 0..4 {
        let signer = PrivateKeySigner::random();
        let vote_owner = signer.address_bytes();
        let mut vote = Vote {
            vote_id: Uuid::new_v4().as_u128() as u32,
            vote_owner: vote_owner.clone(),
            proposal_id: proposal.proposal_id,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Failed to get current time")
                .as_secs(),
            vote: true,
            parent_hash: Vec::new(),
            received_hash: proposal.votes[0].vote_hash.clone(), // Reference previous vote's hash
            vote_hash: Vec::new(),
            signature: Vec::new(),
        };

        // Compute vote hash
        vote.vote_hash = compute_vote_hash(&vote);
        let vote_bytes = vote.encode_to_vec();
        vote.signature = signer
            .local_sign_message(&vote_bytes)
            .await
            .expect("Failed to sign vote");

        let result = consensus_service
            .process_incoming_vote(group_name, vote.clone())
            .await;

        result.expect("Failed to process vote");
    }

    // With 4 out of 5 votes, we should have sufficient votes for consensus
    assert!(
        consensus_service
            .has_sufficient_votes(group_name, proposal.proposal_id)
            .await
    );

    // Subscribe to consensus events and wait for natural consensus
    let proposal_id = proposal.proposal_id;
    let group_name_clone = group_name;

    // Wait for consensus event with timeout
    let timeout_duration = Duration::from_secs(15);
    let consensus_result = tokio::time::timeout(timeout_duration, async {
        while let Ok((event_group_name, event)) = consensus_events.recv().await {
            if event_group_name == group_name_clone {
                match event {
                    ConsensusEvent::ConsensusReached {
                        proposal_id: event_proposal_id,
                        result,
                    } => {
                        if event_proposal_id == proposal_id {
                            println!("Consensus reached for proposal {proposal_id}: {result}");
                            return Ok(result);
                        }
                    }
                    ConsensusEvent::ConsensusFailed {
                        proposal_id: event_proposal_id,
                        reason,
                    } => {
                        if event_proposal_id == proposal_id {
                            println!("Consensus failed for proposal {proposal_id}: {reason}");
                            return Err(format!("Consensus failed: {reason}"));
                        }
                    }
                }
            }
        }
        Err("Event channel closed".to_string())
    })
    .await
    .expect("Timeout waiting for consensus event")
    .expect("Consensus should succeed");

    // Should have consensus result based on 2n/3 threshold
    assert!(consensus_result); // All votes were true, so result should be true
}

#[tokio::test]
async fn test_remove_group_sessions() {
    let consensus_service = ConsensusService::new();

    let group_name = "test_group_remove";
    let expected_voters_count = 2;
    let signer = PrivateKeySigner::random();
    let proposal_owner = signer.address_bytes();

    // Create a proposal
    let proposal = consensus_service
        .create_proposal(
            group_name,
            "Test Proposal".to_string(),
            vec![],
            proposal_owner,
            expected_voters_count,
            300,
            true,
        )
        .await
        .expect("Failed to create proposal");

    let _proposal = consensus_service
        .vote_on_proposal(group_name, proposal.proposal_id, true, signer)
        .await
        .expect("Failed to vote on proposal");

    // Verify proposal exists
    let group_stats = consensus_service.get_group_stats(group_name).await;
    assert_eq!(group_stats.total_sessions, 1);

    // Remove group sessions
    consensus_service.remove_group_sessions(group_name).await;

    // Verify group sessions are removed
    let group_stats_after = consensus_service.get_group_stats(group_name).await;
    assert_eq!(group_stats_after.total_sessions, 0);

    // Verify group is not in active groups
    let active_groups = consensus_service.get_active_groups().await;
    assert!(!active_groups.contains(&group_name.to_string()));
}
