use alloy::signers::local::PrivateKeySigner;
use de_mls::consensus::{compute_vote_hash, ConsensusService};
use de_mls::protos::messages::v1::consensus::v1::Vote;
use de_mls::LocalSigner;
use prost::Message;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn test_realtime_consensus_waiting() {
    // Create consensus service
    let consensus_service = ConsensusService::new();

    let group_name = "test_group_realtime".to_string();
    let expected_voters_count = 3;

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

    println!("Created proposal with ID: {}", proposal.proposal_id);

    // Start a background task that waits for consensus
    let consensus_service_clone = consensus_service.clone();
    let group_name_clone = group_name.clone();
    let proposal_id = proposal.proposal_id;

    let consensus_waiter = tokio::spawn(async move {
        println!("Starting consensus waiter for proposal {proposal_id:?}");
        let result = consensus_service_clone
            .wait_for_consensus(group_name_clone, proposal_id, 10)
            .await;
        println!("Consensus waiter completed with result: {result:?}");
        result
    });

    // Wait a bit to ensure the waiter is running
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Add votes to reach consensus
    let mut previous_vote_hash = proposal.votes[0].vote_hash.clone(); // Start with steward's vote hash

    for i in 1..expected_voters_count {
        let signer = PrivateKeySigner::random();
        let proposal_owner = signer.address().to_string().as_bytes().to_vec();
        let mut vote = Vote {
            vote_id: Uuid::new_v4().as_u128() as u32,
            vote_owner: proposal_owner,
            proposal_id: proposal.proposal_id,
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

        println!("Adding vote {} for proposal {}", i, proposal.proposal_id);
        consensus_service
            .process_incoming_vote(group_name.clone(), vote.clone())
            .await
            .expect("Failed to process vote");

        // Update previous vote hash for next iteration
        previous_vote_hash = vote.vote_hash.clone();

        // Small delay between votes
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for consensus result
    let consensus_result = consensus_waiter.await.expect("Consensus waiter failed");

    // Verify consensus was reached
    assert!(consensus_result.is_ok());
    assert!(consensus_result.unwrap()); // Should be true (yes votes)

    println!("Test completed successfully - consensus reached!");
}

#[tokio::test]
async fn test_consensus_timeout() {
    // Create consensus service
    let consensus_service = ConsensusService::new();

    let group_name = "test_group_timeout".to_string();
    let expected_voters_count = 5;
    let signer = PrivateKeySigner::random();
    let proposal_owner = signer.address().to_string().as_bytes().to_vec();

    // Need 4 votes for consensus
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
            true, // liveness_criteria_yes = true
            signer,
        )
        .await
        .expect("Failed to create proposal");

    println!("Created proposal with ID: {}", proposal.proposal_id);

    // Start consensus waiter with short timeout
    let consensus_service_clone = consensus_service.clone();
    let group_name_clone = group_name.clone();
    let proposal_id = proposal.proposal_id;

    let consensus_waiter = tokio::spawn(async move {
        println!("Starting consensus waiter with timeout for proposal {proposal_id:?}");
        let result = consensus_service_clone
            .wait_for_consensus(group_name_clone, proposal_id, 2) // 2 second timeout
            .await;
        println!("Consensus waiter completed with result: {result:?}");
        result
    });

    // Don't add any additional votes - should timeout

    // Wait for consensus result
    let consensus_result = consensus_waiter.await.expect("Consensus waiter failed");

    // Verify timeout occurred and liveness criteria was applied
    // With liveness_criteria_yes = true, should return Some(true)
    assert!(consensus_result.is_ok());
    assert!(consensus_result.unwrap());

    println!("Test completed successfully - timeout occurred and liveness criteria applied!");
}

#[tokio::test]
async fn test_consensus_with_mixed_votes() {
    // Create consensus service
    let consensus_service = ConsensusService::new();
    let signer = PrivateKeySigner::random();
    let proposal_owner = signer.address().to_string().as_bytes().to_vec();

    let group_name = "test_group_mixed".to_string();
    let expected_voters_count = 3;

    // Create a proposal
    let (proposal, _vote) = consensus_service
        .create_proposal_with_vote(
            group_name.clone(),
            "Test Proposal".to_string(),
            "Test payload".to_string(),
            proposal_owner,
            true, // Steward votes yes
            expected_voters_count,
            300,
            true,
            signer,
        )
        .await
        .expect("Failed to create proposal");

    println!("Created proposal with ID: {}", proposal.proposal_id);

    // Start consensus waiter
    let consensus_service_clone = consensus_service.clone();
    let group_name_clone = group_name.clone();
    let proposal_id = proposal.proposal_id;

    let consensus_waiter = tokio::spawn(async move {
        println!("Starting consensus waiter for proposal {proposal_id:?}");
        let result = consensus_service_clone
            .wait_for_consensus(group_name_clone, proposal_id, 10)
            .await;
        println!("Consensus waiter completed with result: {result:?}");
        result
    });

    // Wait a bit to ensure the waiter is running
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Add mixed votes: one yes, one no
    let votes = vec![(2, false), (3, false)];
    let mut previous_vote_hash = proposal.votes[0].vote_hash.clone(); // Start with steward's vote hash

    for (i, vote_value) in votes {
        let signer = PrivateKeySigner::random();
        let proposal_owner = signer.address().to_string().as_bytes().to_vec();
        let mut vote = Vote {
            vote_id: Uuid::new_v4().as_u128() as u32,
            vote_owner: proposal_owner,
            proposal_id: proposal.proposal_id,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Failed to get current time")
                .as_secs(),
            vote: vote_value,
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

        println!(
            "Adding vote {} (value: {}) for proposal {}",
            i, vote_value, proposal.proposal_id
        );
        consensus_service
            .process_incoming_vote(group_name.clone(), vote.clone())
            .await
            .expect("Failed to process vote");

        // Update previous vote hash for next iteration
        previous_vote_hash = vote.vote_hash.clone();

        // Small delay between votes
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for consensus result
    let consensus_result = consensus_waiter.await.expect("Consensus waiter failed");

    // Verify consensus was reached
    assert!(consensus_result.is_ok());
    // With 2 no votes and 1 yes vote, consensus should be no
    assert!(!consensus_result.unwrap());

    println!("Test completed successfully - consensus reached with mixed votes!");
}

#[tokio::test]
async fn test_rfc_vote_chain_validation() {
    use de_mls::consensus::compute_vote_hash;
    use de_mls::LocalSigner;

    // Create consensus service
    let consensus_service = ConsensusService::new();

    let group_name = "test_rfc_validation".to_string();
    let expected_voters_count = 3;

    let signer1 = PrivateKeySigner::random();
    let signer2 = PrivateKeySigner::random();
    let _signer3 = PrivateKeySigner::random();

    // Create first proposal with steward vote
    let (proposal, _steward_vote) = consensus_service
        .create_proposal_with_vote(
            group_name.clone(),
            "RFC Test Proposal".to_string(),
            "Test payload".to_string(),
            signer1.address().to_string().as_bytes().to_vec(),
            true,
            expected_voters_count,
            300,
            true,
            signer1.clone(),
        )
        .await
        .expect("Failed to create proposal");

    println!("Created proposal with ID: {}", proposal.proposal_id);

    // Create second vote from different voter
    let mut vote2 = Vote {
        vote_id: Uuid::new_v4().as_u128() as u32,
        vote_owner: signer2.address().to_string().as_bytes().to_vec(),
        proposal_id: proposal.proposal_id,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Failed to get current time")
            .as_secs(),
        vote: true,
        parent_hash: Vec::new(), // Different voter, no parent
        received_hash: proposal.votes[0].vote_hash.clone(), // Should be hash of first vote
        vote_hash: Vec::new(),
        signature: Vec::new(),
    };

    // Compute vote hash and signature
    vote2.vote_hash = compute_vote_hash(&vote2);
    let vote2_bytes = vote2.encode_to_vec();
    vote2.signature = signer2
        .local_sign_message(&vote2_bytes)
        .await
        .expect("Failed to sign vote");

    // Create proposal with two votes from different voters
    let mut test_proposal = proposal.clone();
    test_proposal.votes.push(vote2.clone());

    // Validate the proposal - should pass RFC validation
    let validation_result = consensus_service.validate_proposal(&test_proposal);
    assert!(
        validation_result.is_ok(),
        "RFC validation should pass: {validation_result:?}"
    );

    // Test invalid vote chain (wrong received_hash)
    let mut invalid_proposal = test_proposal.clone();
    invalid_proposal.votes[1].received_hash = vec![0; 32]; // Wrong hash

    let invalid_result = consensus_service.validate_proposal(&invalid_proposal);
    assert!(
        invalid_result.is_err(),
        "Invalid vote chain should be rejected"
    );

    println!("RFC vote chain validation test completed successfully!");
}
