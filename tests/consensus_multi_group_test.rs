use alloy::signers::local::PrivateKeySigner;
use de_mls::consensus::{compute_vote_hash, ConsensusConfig, ConsensusService};
use de_mls::protos::messages::v1::consensus::v1::Vote;
use de_mls::LocalSigner;
use prost::Message;

#[tokio::test]
async fn test_basic_consensus_service() {
    // Create consensus service
    let config = ConsensusConfig::default();
    let consensus_service = ConsensusService::new(config);

    let group_name = "test_group".to_string();
    let expected_voters_count = 3;

    let signer = PrivateKeySigner::random();
    let proposal_owner_address = signer.address();
    let proposal_owner = proposal_owner_address.to_string().as_bytes().to_vec();

    // Create a proposal
    let (proposal, _vote) = consensus_service
        .create_proposal_with_vote(
            group_name.clone(),
            "Test Proposal".to_string(),
            "Test payload".to_string(),
            proposal_owner,
            true,
            expected_voters_count,
            300,
            true,
            signer,
        )
        .await
        .expect("Failed to create proposal");

    // Verify proposal was created
    let active_proposals = consensus_service
        .get_active_proposals(group_name.clone())
        .await;
    assert_eq!(active_proposals.len(), 1);
    assert_eq!(active_proposals[0].proposal_id, proposal.proposal_id);

    // Verify group statistics
    let group_stats = consensus_service.get_group_stats(group_name.clone()).await;
    assert_eq!(group_stats.total_sessions, 1);
    assert_eq!(group_stats.active_sessions, 1);

    // Verify consensus threshold calculation
    // With 3 expected voters, we need 2n/3 = 2 votes for consensus
    // Initially we have 1 vote (steward), so we don't have sufficient votes
    assert!(
        !consensus_service
            .has_sufficient_votes(group_name.clone(), proposal.proposal_id)
            .await
    );

    let signer_2 = PrivateKeySigner::random();
    let proposal_owner_2 = signer_2.address().to_string().as_bytes().to_vec();
    // Add 1 more vote (total 2 votes)
    let mut vote = Vote {
        vote_id: proposal.proposal_id,
        vote_owner: proposal_owner_2,
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
        .process_incoming_vote(group_name.clone(), vote)
        .await
        .expect("Failed to process vote");

    // Now we should have sufficient votes (2 out of 3 expected voters)
    assert!(
        consensus_service
            .has_sufficient_votes(group_name.clone(), proposal.proposal_id)
            .await
    );
}

#[tokio::test]
async fn test_multi_group_consensus_service() {
    // Create consensus service with max 10 sessions per group
    let config = ConsensusConfig::default();
    let consensus_service = ConsensusService::new_with_max_sessions(config, 10);

    // Test group 1
    let group1_name = "test_group_1".to_string();
    let group1_members_count = 3;
    let signer_1 = PrivateKeySigner::random();
    let proposal_owner_1 = signer_1.address().to_string().as_bytes().to_vec();

    // Test group 2
    let group2_name = "test_group_2".to_string();
    let group2_members_count = 3;
    let signer_2 = PrivateKeySigner::random();
    let proposal_owner_2 = signer_2.address().to_string().as_bytes().to_vec();

    // Create proposals for group 1
    let (_proposal1, _vote1) = consensus_service
        .create_proposal_with_vote(
            group1_name.clone(),
            "Proposal 1".to_string(),
            "Test payload 1".to_string(),
            proposal_owner_1,
            true,
            group1_members_count,
            300,
            true,
            signer_1,
        )
        .await
        .expect("Failed to create proposal 1");

    let (_proposal2, _vote2) = consensus_service
        .create_proposal_with_vote(
            group1_name.clone(),
            "Proposal 2".to_string(),
            "Test payload 2".to_string(),
            proposal_owner_2.clone(),
            false,
            group1_members_count,
            300,
            true,
            signer_2.clone(),
        )
        .await
        .expect("Failed to create proposal 2");

    // Create proposal for group 2
    let (_proposal3, _vote3) = consensus_service
        .create_proposal_with_vote(
            group2_name.clone(),
            "Proposal 3".to_string(),
            "Test payload 3".to_string(),
            proposal_owner_2,
            true,
            group2_members_count,
            300,
            true,
            signer_2,
        )
        .await
        .expect("Failed to create proposal 3");

    // Verify proposals are created for both groups
    let group1_proposals = consensus_service
        .get_active_proposals(group1_name.clone())
        .await;
    let group2_proposals = consensus_service
        .get_active_proposals(group2_name.clone())
        .await;

    assert_eq!(group1_proposals.len(), 2);
    assert_eq!(group2_proposals.len(), 1);

    // Verify group statistics
    let group1_stats = consensus_service.get_group_stats(group1_name.clone()).await;
    let group2_stats = consensus_service.get_group_stats(group2_name.clone()).await;

    assert_eq!(group1_stats.total_sessions, 2);
    assert_eq!(group1_stats.active_sessions, 2);
    assert_eq!(group2_stats.total_sessions, 1);
    assert_eq!(group2_stats.active_sessions, 1);

    // Verify overall statistics
    let overall_stats = consensus_service.get_overall_stats().await;
    assert_eq!(overall_stats.total_sessions, 3);
    assert_eq!(overall_stats.active_sessions, 3);

    // Verify active groups
    let active_groups = consensus_service.get_active_groups().await;
    assert_eq!(active_groups.len(), 2);
    assert!(active_groups.contains(&group1_name));
    assert!(active_groups.contains(&group2_name));
}

#[tokio::test]
async fn test_consensus_threshold_calculation() {
    let config = ConsensusConfig::default();
    let consensus_service = ConsensusService::new(config);

    let group_name = "test_group_threshold".to_string();
    let expected_voters_count = 5;
    let signer = PrivateKeySigner::random();
    let proposal_owner = signer.address().to_string().as_bytes().to_vec();

    // Create a proposal
    let (proposal, _vote) = consensus_service
        .create_proposal_with_vote(
            group_name.clone(),
            "Test Proposal".to_string(),
            "Test payload".to_string(),
            proposal_owner,
            true,
            expected_voters_count,
            300,
            true,
            signer,
        )
        .await
        .expect("Failed to create proposal");

    // With 5 expected voters, we need 2n/3 = 3.33... -> 4 votes for consensus
    // Initially we have 1 vote (steward), so we don't have sufficient votes
    assert!(
        !consensus_service
            .has_sufficient_votes(group_name.clone(), proposal.proposal_id)
            .await
    );

    // Add 3 more votes (total 4 votes) - this should reach consensus after timeout
    // because 4 out of 5 votes meets the 2n/3 threshold
    let mut previous_vote_hash = proposal.votes[0].vote_hash.clone(); // Start with steward's vote hash

    for _ in 2..5 {
        let signer = PrivateKeySigner::random();
        let proposal_owner = signer.address().to_string().as_bytes().to_vec();
        let mut vote = Vote {
            vote_id: proposal.proposal_id,
            vote_owner: proposal_owner,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Failed to get current time")
                .as_secs(),
            vote: true,
            parent_hash: Vec::new(),
            received_hash: previous_vote_hash.clone(), // Reference previous vote's hash
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
            .process_incoming_vote(group_name.clone(), vote.clone())
            .await;

        result.expect("Failed to process vote");

        // Update previous vote hash for next iteration
        previous_vote_hash = vote.vote_hash.clone();
    }

    // With 4 out of 5 votes, we should have sufficient votes for consensus
    assert!(
        consensus_service
            .has_sufficient_votes(group_name.clone(), proposal.proposal_id)
            .await
    );

    // Wait for consensus with timeout - should return result based on threshold
    let consensus_result = consensus_service
        .wait_for_consensus(group_name.clone(), proposal.proposal_id, 5)
        .await;

    // Should have consensus result based on 2n/3 threshold
    assert!(consensus_result.is_ok());
    assert!(consensus_result.unwrap()); // All votes were true
}

#[tokio::test]
async fn test_remove_group_sessions() {
    let config = ConsensusConfig::default();
    let consensus_service = ConsensusService::new(config);

    let group_name = "test_group_remove".to_string();
    let expected_voters_count = 2;
    let signer = PrivateKeySigner::random();
    let proposal_owner = signer.address().to_string().as_bytes().to_vec();

    // Create a proposal
    let (_proposal, _vote) = consensus_service
        .create_proposal_with_vote(
            group_name.clone(),
            "Test Proposal".to_string(),
            "Test payload".to_string(),
            proposal_owner,
            true,
            expected_voters_count,
            300,
            true,
            signer,
        )
        .await
        .expect("Failed to create proposal");

    // Verify proposal exists
    let group_stats = consensus_service.get_group_stats(group_name.clone()).await;
    assert_eq!(group_stats.total_sessions, 1);

    // Remove group sessions
    consensus_service
        .remove_group_sessions(group_name.clone())
        .await;

    // Verify group sessions are removed
    let group_stats_after = consensus_service.get_group_stats(group_name.clone()).await;
    assert_eq!(group_stats_after.total_sessions, 0);

    // Verify group is not in active groups
    let active_groups = consensus_service.get_active_groups().await;
    assert!(!active_groups.contains(&group_name));
}
