use de_mls::{error::GroupError, group::Group, state_machine::GroupState};
use mls_crypto::identity::random_identity;
use mls_crypto::openmls_provider::MlsProvider;

#[tokio::test]
async fn test_state_machine_transitions() {
    let crypto = MlsProvider::default();
    let mut id_steward = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group",
        true,
        Some(&crypto),
        Some(id_steward.signer()),
        Some(&id_steward.credential_with_key()),
    )
    .expect("Failed to create group");

    // Initial state should be Working
    assert_eq!(group.get_state().await, GroupState::Working);

    // Test start_steward_epoch_with_validation
    let proposal_count = group
        .start_steward_epoch_with_validation()
        .await
        .expect("Failed to start steward epoch");
    assert_eq!(proposal_count, 0); // No proposals initially
    assert_eq!(group.get_state().await, GroupState::Working); // Should stay in Working

    // Add some proposals
    let kp_user = id_steward
        .generate_key_package(&crypto)
        .expect("Failed to generate key package");
    group
        .store_invite_proposal(Box::new(kp_user))
        .await
        .expect("Failed to store proposal");

    // Now start steward epoch with proposals
    let proposal_count = group
        .start_steward_epoch_with_validation()
        .await
        .expect("Failed to start steward epoch");
    assert_eq!(proposal_count, 1); // Should have 1 proposal
    assert_eq!(group.get_state().await, GroupState::Waiting);

    // Test start_voting_with_validation
    group.start_voting().await.expect("Failed to start voting");
    assert_eq!(group.get_state().await, GroupState::Voting);

    // Test complete_voting with success
    group
        .complete_voting(true)
        .await
        .expect("Failed to complete voting");
    assert_eq!(group.get_state().await, GroupState::ConsensusReached);

    // Test start_waiting_after_consensus
    group
        .start_waiting_after_consensus()
        .await
        .expect("Failed to start waiting after consensus");
    assert_eq!(group.get_state().await, GroupState::Waiting);

    // Test apply_proposals_and_complete
    group
        .handle_yes_vote()
        .await
        .expect("Failed to apply proposals");
    assert_eq!(group.get_state().await, GroupState::Working);
    assert_eq!(group.get_pending_proposals_count().await, 0);
}

#[tokio::test]
async fn test_invalid_state_transitions() {
    let crypto = MlsProvider::default();
    let mut id_steward = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group",
        true,
        Some(&crypto),
        Some(id_steward.signer()),
        Some(&id_steward.credential_with_key()),
    )
    .expect("Failed to create group");

    // Cannot complete voting from Working state
    let result = group.complete_voting(true).await;
    assert!(matches!(
        result,
        Err(GroupError::InvalidStateTransition { .. })
    ));

    // Cannot apply proposals from Working state
    let result = group.handle_yes_vote().await;
    assert!(matches!(
        result,
        Err(GroupError::InvalidStateTransition { .. })
    ));

    // Start steward epoch - but there are no proposals, so it should stay in Working state
    let proposal_count = group
        .start_steward_epoch_with_validation()
        .await
        .expect("Failed to start steward epoch");
    assert_eq!(proposal_count, 0); // No proposals
    assert_eq!(group.get_state().await, GroupState::Working); // Should still be in Working state

    // Cannot apply proposals from Working state (even after steward epoch start with no proposals)
    let result = group.handle_yes_vote().await;
    assert!(matches!(
        result,
        Err(GroupError::InvalidStateTransition { .. })
    ));

    // Add a proposal to actually transition to Waiting state
    let kp_user = id_steward
        .generate_key_package(&crypto)
        .expect("Failed to generate key package");
    group
        .store_invite_proposal(Box::new(kp_user))
        .await
        .expect("Failed to store proposal");

    // Now start steward epoch with proposals - should transition to Waiting
    let proposal_count = group
        .start_steward_epoch_with_validation()
        .await
        .expect("Failed to start steward epoch");
    assert_eq!(proposal_count, 1); // Should have 1 proposal
    assert_eq!(group.get_state().await, GroupState::Waiting); // Should now be in Waiting state

    // Can apply proposals from Waiting state (even with no proposals)
    let result = group.handle_yes_vote().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_proposal_counting() {
    let crypto = MlsProvider::default();
    let id_steward = random_identity().expect("Failed to create identity");
    let mut id_user = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group",
        true,
        Some(&crypto),
        Some(id_steward.signer()),
        Some(&id_steward.credential_with_key()),
    )
    .expect("Failed to create group");

    // Add some proposals
    let kp_user = id_user
        .generate_key_package(&crypto)
        .expect("Failed to generate key package");

    group
        .store_invite_proposal(Box::new(kp_user.clone()))
        .await
        .expect("Failed to store proposal");
    group
        .store_remove_proposal("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string())
        .await
        .expect("Failed to put remove proposal");

    // Start steward epoch - should collect proposals
    let proposal_count = group
        .start_steward_epoch_with_validation()
        .await
        .expect("Failed to start steward epoch");
    assert_eq!(proposal_count, 2); // Should have 2 proposals
    assert_eq!(group.get_state().await, GroupState::Waiting);
    assert_eq!(group.get_voting_proposals_count().await, 2);

    // Complete the flow
    group.start_voting().await.expect("Failed to start voting");
    group
        .complete_voting(true)
        .await
        .expect("Failed to complete voting");
    group
        .handle_yes_vote()
        .await
        .expect("Failed to apply proposals");

    // Proposals count should be reset
    assert_eq!(group.get_voting_proposals_count().await, 0);
    assert_eq!(group.get_pending_proposals_count().await, 0);
}

#[tokio::test]
async fn test_steward_validation() {
    let _crypto = MlsProvider::default();
    let _id_steward = random_identity().expect("Failed to create identity");

    // Create group without steward
    let mut group = Group::new(
        "test_group",
        false, // No steward
        None,
        None,
        None,
    )
    .expect("Failed to create group");

    // Should fail to start steward epoch without steward
    let result = group.start_steward_epoch_with_validation().await;
    assert!(matches!(result, Err(GroupError::StewardNotSet)));
}

#[tokio::test]
async fn test_consensus_result_handling() {
    let crypto = MlsProvider::default();
    let id_steward = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group",
        true,
        Some(&crypto),
        Some(id_steward.signer()),
        Some(&id_steward.credential_with_key()),
    )
    .expect("Failed to create group");

    // Start steward epoch and voting
    group
        .start_steward_epoch_with_validation()
        .await
        .expect("Failed to start steward epoch");
    group.start_voting().await.expect("Failed to start voting");

    // Test consensus result handling for steward
    let result = group.complete_voting(true).await;
    assert!(result.is_ok());
    assert_eq!(group.get_state().await, GroupState::ConsensusReached);

    // Test invalid consensus result handling (not in voting state)
    let result = group.complete_voting(true).await;
    assert!(matches!(
        result,
        Err(GroupError::InvalidStateTransition { .. })
    ));
}

#[tokio::test]
async fn test_voting_validation_edge_cases() {
    let _crypto = MlsProvider::default();
    let _id_steward = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group",
        true,
        Some(&_crypto),
        Some(_id_steward.signer()),
        Some(&_id_steward.credential_with_key()),
    )
    .expect("Failed to create group");

    // Test starting voting from Working state (should transition to Waiting first)
    group.start_voting().await.expect("Failed to start voting");
    assert_eq!(group.get_state().await, GroupState::Voting);

    // Test starting voting from Voting state (should fail)
    let result = group.start_voting().await;
    assert!(matches!(
        result,
        Err(GroupError::InvalidStateTransition { .. })
    ));
}
