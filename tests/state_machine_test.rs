use de_mls::{error::GroupError, group::Group, state_machine::GroupState};
use mls_crypto::identity::random_identity;
use mls_crypto::openmls_provider::MlsProvider;

#[tokio::test]
async fn test_state_machine_transitions() {
    let crypto = MlsProvider::default();
    let id_steward = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group".to_string(),
        true,
        Some(&crypto),
        Some(id_steward.signer()),
        Some(&id_steward.credential_with_key()),
    )
    .expect("Failed to create group");

    // Initial state should be Working
    assert_eq!(group.get_state().await, GroupState::Working);

    // Test start_steward_epoch
    group
        .start_steward_epoch()
        .await
        .expect("Failed to start steward epoch");
    assert_eq!(group.get_state().await, GroupState::Waiting);

    // Test start_voting
    group.start_voting().await.expect("Failed to start voting");
    assert_eq!(group.get_state().await, GroupState::Voting);

    // Test complete_voting with success
    group
        .complete_voting(true)
        .await
        .expect("Failed to complete voting");
    assert_eq!(group.get_state().await, GroupState::Waiting);

    // Test apply_proposals
    group
        .remove_proposals_and_complete()
        .await
        .expect("Failed to remove proposals");
    assert_eq!(group.get_state().await, GroupState::Working);
    assert_eq!(group.get_pending_proposals_count().await, 0);
}

#[tokio::test]
async fn test_state_machine_permissions() {
    let crypto = MlsProvider::default();
    let id_steward = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group".to_string(),
        true,
        Some(&crypto),
        Some(id_steward.signer()),
        Some(&id_steward.credential_with_key()),
    )
    .expect("Failed to create group");

    // Working state - anyone can send messages
    assert!(group.can_send_message(false, false).await); // Regular user, no proposals
    assert!(group.can_send_message(true, false).await); // Steward, no proposals
    assert!(group.can_send_message(true, true).await); // Steward, with proposals

    // Start steward epoch
    group
        .start_steward_epoch()
        .await
        .expect("Failed to start steward epoch");

    // Waiting state - only steward with proposals can send messages
    assert!(!group.can_send_message(false, false).await); // Regular user, no proposals
    assert!(!group.can_send_message(false, true).await); // Regular user, with proposals
    assert!(!group.can_send_message(true, false).await); // Steward, no proposals
    assert!(group.can_send_message(true, true).await); // Steward, with proposals

    // Start voting
    group.start_voting().await.expect("Failed to start voting");

    // Voting state - everyone can send messages
    assert!(group.can_send_message(false, false).await);
    assert!(group.can_send_message(false, true).await);
    assert!(group.can_send_message(true, false).await);
    assert!(group.can_send_message(true, true).await);
}

#[tokio::test]
async fn test_invalid_state_transitions() {
    let crypto = MlsProvider::default();
    let id_steward = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group".to_string(),
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
    let result = group.remove_proposals_and_complete().await;
    assert!(matches!(
        result,
        Err(GroupError::InvalidStateTransition { .. })
    ));

    // Start steward epoch
    group
        .start_steward_epoch()
        .await
        .expect("Failed to start steward epoch");

    // Can apply proposals from Waiting state (even with no proposals)
    let result = group.remove_proposals_and_complete().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_proposal_counting() {
    let crypto = MlsProvider::default();
    let id_steward = random_identity().expect("Failed to create identity");
    let mut id_user = random_identity().expect("Failed to create identity");

    let mut group = Group::new(
        "test_group".to_string(),
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
        .store_remove_proposal(vec![1, 2, 3])
        .await
        .expect("Failed to put remove proposal");

    // Start steward epoch - should collect proposals
    group
        .start_steward_epoch()
        .await
        .expect("Failed to start steward epoch");
    assert_eq!(group.get_voting_proposals_count().await, 2);

    // Complete the flow
    group.start_voting().await.expect("Failed to start voting");
    group
        .complete_voting(true)
        .await
        .expect("Failed to complete voting");
    group
        .remove_proposals_and_complete()
        .await
        .expect("Failed to remove proposals");

    // Proposals count should be reset
    assert_eq!(group.get_voting_proposals_count().await, 0);
    assert_eq!(group.get_pending_proposals_count().await, 0);
}
