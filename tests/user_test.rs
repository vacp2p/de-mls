use de_mls::{
    consensus::ConsensusService,
    protos::messages::v1::app_message,
    state_machine::GroupState,
    user::{User, UserAction},
};
use ds::waku_actor::WakuMessageToSend;
use waku_bindings::WakuMessage;

const EXPECTED_EPOCH_1: u64 = 1;
const EXPECTED_EPOCH_2: u64 = 2;

const EXPECTED_MEMBERS_2: usize = 2;
const EXPECTED_MEMBERS_3: usize = 3;

const ALICE_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
// const ALICE_WALLET_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const BOB_PRIVATE_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
// const BOB_WALLET_ADDRESS: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const CAROL_PRIVATE_KEY: &str =
    "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
// const CAROL_WALLET_ADDRESS: &str = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";

const GROUP_NAME: &str = "new_group";

async fn create_two_test_user_with_group(group_name: &str) -> (User, User) {
    let consensus_service = ConsensusService::new();
    let mut alice =
        User::new(ALICE_PRIVATE_KEY, &consensus_service).expect("Failed to create user for Alice");
    alice
        .create_group(group_name, true)
        .await
        .expect("Failed to create group for Alice");

    let consensus_service = ConsensusService::new();
    let mut bob =
        User::new(BOB_PRIVATE_KEY, &consensus_service).expect("Failed to create user for Bob");
    bob.create_group(group_name, false)
        .await
        .expect("Failed to create group for Bob");

    (alice, bob)
}

async fn create_three_test_user_with_group(group_name: &str) -> (User, User, User) {
    let consensus_service = ConsensusService::new();
    let mut alice =
        User::new(ALICE_PRIVATE_KEY, &consensus_service).expect("Failed to create user");
    alice
        .create_group(group_name, true)
        .await
        .expect("Failed to create group for Alice");

    let consensus_service = ConsensusService::new();
    let mut bob = User::new(BOB_PRIVATE_KEY, &consensus_service).expect("Failed to create user");
    bob.create_group(group_name, false)
        .await
        .expect("Failed to create group for Bob");

    let consensus_service = ConsensusService::new();
    let mut carol =
        User::new(CAROL_PRIVATE_KEY, &consensus_service).expect("Failed to create user");
    carol
        .create_group(group_name, false)
        .await
        .expect("Failed to create group for Carol");

    (alice, bob, carol)
}

async fn get_group_announcement_message(steward: &mut User, group_name: &str) -> WakuMessage {
    steward
        .prepare_steward_msg(group_name)
        .await
        .expect("Failed to prepare steward message")
        .build_waku_message()
        .expect("Failed to build waku message with group announcement")
}

async fn share_group_announcement_for_one_user(
    steward: &mut User,
    invite_user: &mut User,
    group_announcement_message: WakuMessage,
) {
    // Newcomer parse GA message and share his KP to Steward
    let invite_user_kp_message = match invite_user
        .process_waku_message(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message with group announcement")
    {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let invite_user_kp_waku_message = invite_user_kp_message
        .build_waku_message()
        .expect("Failed to build waku message with invite user's KP");

    // Steward parse invite user's KP and add it to the queue of income key packages
    let _steward_action = steward
        .process_waku_message(invite_user_kp_waku_message)
        .await
        .expect("Failed to process waku message with invite user's KP");
}

// In this function, we are starting steward epoch without any user in the group
// So as result we don't expect another vote from another user
// and have `GroupState::Working` after processing steward vote
async fn steward_epoch_without_user_in_group(
    steward: &mut User,
    group_name: &str,
) -> Vec<WakuMessage> {
    // Set up consensus event subscription before voting
    let mut consensus_events = steward.subscribe_to_consensus_events();

    // State machine: start steward epoch, voting, complete voting
    let steward_epoch_proposals = steward
        .start_steward_epoch(group_name)
        .await
        .expect("Failed to start steward epoch");

    println!("Debug: Steward epoch returned {steward_epoch_proposals} proposals");

    let (proposal_id, action) = steward
        .get_proposals_for_steward_voting(group_name)
        .await
        .expect("Failed to start voting");

    println!("Debug: Proposal ID: {proposal_id}");

    // This message will be printed to the app and allow steward to vote
    let _steward_voting_proposal_app_message = match action {
        UserAction::SendToApp(app_msg) => app_msg,
        _ => panic!("User action is not SendToWaku"),
    };

    let steward_state = steward
        .get_group_state(group_name)
        .await
        .expect("Failed to get group state for steward after making voting proposal");
    println!("Debug: Steward state after making voting proposal: {steward_state:?}");
    assert_eq!(steward_state, GroupState::Voting);

    // Now steward can vote
    steward
        .process_user_vote(proposal_id, true, group_name)
        .await
        .expect("Failed to process steward vote on proposal");

    let mut msgs_to_send: Vec<WakuMessageToSend> = vec![];
    // Process any consensus events that were emitted during voting
    while let Ok((group_name, ev)) = consensus_events.try_recv() {
        println!(
            "Debug: Processing consensus event in steward_epoch: {ev:?} for group {group_name}"
        );

        let wmts = steward
            .handle_consensus_event(&group_name, ev)
            .await
            .expect("Failed to handle consensus event");
        msgs_to_send.extend(wmts);
    }

    let steward_state = steward
        .get_group_state(group_name)
        .await
        .expect("Failed to get group state for steward after voting");
    println!("Debug: Steward state after voting: {steward_state:?}");
    assert_eq!(steward_state, GroupState::Working);

    let mut res_msgs_to_send: Vec<WakuMessage> = vec![];
    for msg in msgs_to_send {
        let waku_message = msg
            .build_waku_message()
            .expect("Failed to build waku message");
        res_msgs_to_send.push(waku_message);
    }

    res_msgs_to_send
}

// In this function, we are starting steward epoch with users in the group
// So as result we expect another vote from another user
// and have `GroupState::Working` after processing steward vote
async fn steward_epoch_with_user_in_group(
    steward: &mut User,
    group_name: &str,
) -> (Vec<WakuMessage>, u32) {
    // Set up consensus event subscription before voting
    let mut consensus_events = steward.subscribe_to_consensus_events();

    // State machine: start steward epoch, voting, complete voting
    let steward_epoch_proposals = steward
        .start_steward_epoch(group_name)
        .await
        .expect("Failed to start steward epoch");

    println!("Debug: Steward epoch returned {steward_epoch_proposals} proposals");

    let (proposal_id, action) = steward
        .get_proposals_for_steward_voting(group_name)
        .await
        .expect("Failed to start voting");

    println!("Debug: Proposal ID: {proposal_id}");

    // This message will be printed to the app and allow steward to vote
    let _steward_voting_proposal_app_message = match action {
        UserAction::SendToApp(app_msg) => app_msg,
        _ => panic!("User action is not SendToWaku"),
    };

    let steward_state = steward
        .get_group_state(group_name)
        .await
        .expect("Failed to get group state for steward after making voting proposal");
    println!("Debug: Steward state after making voting proposal: {steward_state:?}");
    assert_eq!(steward_state, GroupState::Voting);

    // Now steward can vote
    let steward_action = steward
        .process_user_vote(proposal_id, true, group_name)
        .await
        .expect("Failed to process steward vote on proposal");

    let mut msgs_to_send: Vec<WakuMessageToSend> = vec![];
    // Process any consensus events that were emitted during voting
    while let Ok((group_name, ev)) = consensus_events.try_recv() {
        println!(
            "Debug: Processing consensus event in steward_epoch: {ev:?} for group {group_name}"
        );

        let wmts = steward
            .handle_consensus_event(&group_name, ev)
            .await
            .expect("Failed to handle consensus event");
        msgs_to_send.extend(wmts);
    }

    let steward_state = steward
        .get_group_state(group_name)
        .await
        .expect("Failed to get group state for steward after voting");
    println!("Debug: Steward state after voting: {steward_state:?}");
    assert_eq!(steward_state, GroupState::Voting);

    let steward_voting_proposal_waku_message = match steward_action {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };

    // Build the voting proposal message for other users
    let voting_proposal_waku_message = steward_voting_proposal_waku_message
        .build_waku_message()
        .expect("Failed to build waku message with steward voting proposal");

    (vec![voting_proposal_waku_message], proposal_id)
}

async fn user_join_group(user: &mut User, welcome_message: WakuMessage) {
    let user_res_action = user
        .process_waku_message(welcome_message)
        .await
        .expect("Failed to process waku message for user to join the group");

    match user_res_action {
        UserAction::SendToWaku(_) => {
            println!("Debug: user join group");
        }
        _ => panic!("User action is not SendToWaku: {user_res_action:?}"),
    };
}

async fn user_vote_on_proposal(
    user: &mut User,
    waku_proposal_message: WakuMessage,
    vote: bool,
    group_name: &str,
) -> WakuMessage {
    println!("Debug: user vote on proposal: {waku_proposal_message:?}");
    let mut consensus_events = user.subscribe_to_consensus_events();
    let user_action = user
        .process_waku_message(waku_proposal_message)
        .await
        .expect("Failed to process waku message for user to vote on proposal");

    let msg = match user_action {
        UserAction::SendToApp(msg) => {
            println!("Debug: user got message: {msg:?}");
            msg
        }
        _ => panic!("User action is not SendToApp or DoNothing: {user_action:?}"),
    };

    let user_state = user
        .get_group_state(group_name)
        .await
        .expect("Failed to get group state for user after making voting proposal");
    println!("Debug: User state after making voting proposal: {user_state:?}");
    assert_eq!(user_state, GroupState::Voting);

    let proposal_id = match msg.payload {
        Some(app_message::Payload::VotingProposal(proposal)) => proposal.proposal_id,
        _ => panic!("User got an unexpected message: {msg:?}"),
    };

    // after getting voting proposal, user actually should send it into app and get vote result
    // here we mock it and start to process user vote
    let user_action = user
        .process_user_vote(proposal_id, vote, group_name)
        .await
        .expect("Failed to process steward vote on proposal");

    let mut msgs_to_send: Vec<WakuMessageToSend> = vec![];
    while let Ok((group_name, ev)) = consensus_events.try_recv() {
        println!(
            "Debug: Processing consensus event in user_vote_on_proposal: {ev:?} for group {group_name}"
        );

        let wmts = user
            .handle_consensus_event(&group_name, ev)
            .await
            .expect("Failed to handle consensus event");
        msgs_to_send.extend(wmts);
    }

    let user_state = user
        .get_group_state(group_name)
        .await
        .expect("Failed to get group state for user after vote");
    println!("Debug: User state after vote: {user_state:?}");
    assert_eq!(user_state, GroupState::Waiting);

    let msg = match user_action {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku: {user_action:?}"),
    };

    msg.build_waku_message()
        .expect("Failed to build waku message")
}

async fn process_and_handle_trigger_event_message(
    user: &mut User,
    waku_message: WakuMessage,
    expected_group_state: GroupState,
) -> Vec<WakuMessage> {
    let mut consensus_events = user.subscribe_to_consensus_events();

    user.process_waku_message(waku_message)
        .await
        .expect("Failed to process waku message");

    let mut msgs_to_send: Vec<WakuMessageToSend> = vec![];
    // Process any consensus events that were emitted during voting
    while let Ok((group_name, ev)) = consensus_events.try_recv() {
        println!(
            "Debug: Processing consensus event in steward_epoch: {ev:?} for group {group_name}"
        );

        let wmts = user
            .handle_consensus_event(&group_name, ev)
            .await
            .expect("Failed to handle consensus event");
        msgs_to_send.extend(wmts);
    }

    let mut waku_msgs_to_send: Vec<WakuMessage> = vec![];
    for msg in msgs_to_send {
        let waku_msg = msg
            .build_waku_message()
            .expect("Failed to build waku message");
        waku_msgs_to_send.push(waku_msg);
    }

    let user_state = user
        .get_group_state(GROUP_NAME)
        .await
        .expect("Failed to get group state for user after processing trigger event message");
    println!("Debug: User state after voting: {user_state:?}");
    assert_eq!(user_state, expected_group_state);

    waku_msgs_to_send
}

async fn check_users_have_same_group_stats(
    alice: &User,
    bob: &User,
    group_name: &str,
    expected_members: usize,
    expected_epoch: u64,
) {
    let alice_group_state = alice
        .get_group_state(group_name)
        .await
        .expect("Failed to get group state for Alice");
    assert_eq!(alice_group_state, GroupState::Working);
    let bob_group_state = bob
        .get_group_state(group_name)
        .await
        .expect("Failed to get group state for Bob");
    assert_eq!(bob_group_state, GroupState::Working);

    let bob_members = bob
        .get_group_number_of_members(group_name)
        .await
        .expect("Failed to get number of members for Bob");
    let bob_epoch = bob
        .get_group_mls_epoch(group_name)
        .await
        .expect("Failed to get MLS epoch for Bob");
    assert_eq!(
        bob_members, expected_members,
        "Wrong number of members in the group for Bob"
    );
    assert_eq!(
        bob_epoch, expected_epoch,
        "Bob group epoch is not {expected_epoch}"
    );

    let alice_members = alice
        .get_group_number_of_members(group_name)
        .await
        .expect("Failed to get number of members for Alice");
    let alice_epoch = alice
        .get_group_mls_epoch(group_name)
        .await
        .expect("Failed to get MLS epoch for Alice");
    assert_eq!(
        alice_members, expected_members,
        "Wrong number of members in the group for Alice"
    );
    assert_eq!(
        alice_epoch, expected_epoch,
        "Alice group epoch is not {expected_epoch}"
    );

    assert_eq!(
        bob_members, alice_members,
        "Bob and Alice have different members"
    );
    assert_eq!(
        bob_epoch, alice_epoch,
        "Bob and Alice have different MLS epochs"
    );
}

// async fn create_ban_request_message(user: &mut User, group_name: &str) -> WakuMessage {
//     let ban_request_msg = BanRequest {
//         user_to_ban: CAROL_WALLET_ADDRESS.to_string(),
//         requester: ALICE_WALLET_ADDRESS.to_string(), // The current user is the requester
//         group_name: group_name.to_string(),
//     };

//     let waku_msg = user
//         .process_ban_request(ban_request_msg, group_name)
//         .await
//         .expect("Failed to process ban request");

//     let waku_msg = waku_msg
//         .build_waku_message()
//         .expect("Failed to build waku message");
//     waku_msg
// }

#[tokio::test]
async fn test_invite_user_to_group_flow() {
    let (mut steward, mut user) = create_two_test_user_with_group(GROUP_NAME).await;

    let ga_message = get_group_announcement_message(&mut steward, GROUP_NAME).await;

    share_group_announcement_for_one_user(&mut steward, &mut user, ga_message.clone()).await;

    let steward_epoch_messages =
        steward_epoch_without_user_in_group(&mut steward, GROUP_NAME).await;

    user_join_group(&mut user, steward_epoch_messages[1].clone()).await;

    check_users_have_same_group_stats(
        &steward,
        &user,
        GROUP_NAME,
        EXPECTED_MEMBERS_2,
        EXPECTED_EPOCH_1,
    )
    .await;
}

#[tokio::test]
async fn test_invite_users_to_group_same_epoch_flow() {
    let (mut steward, mut user, mut user2) = create_three_test_user_with_group(GROUP_NAME).await;

    let ga_message = get_group_announcement_message(&mut steward, GROUP_NAME).await;

    share_group_announcement_for_one_user(&mut steward, &mut user, ga_message.clone()).await;
    share_group_announcement_for_one_user(&mut steward, &mut user2, ga_message.clone()).await;

    let steward_epoch_messages =
        steward_epoch_without_user_in_group(&mut steward, GROUP_NAME).await;

    user_join_group(&mut user, steward_epoch_messages[1].clone()).await;
    user_join_group(&mut user2, steward_epoch_messages[1].clone()).await;

    check_users_have_same_group_stats(
        &steward,
        &user,
        GROUP_NAME,
        EXPECTED_MEMBERS_3,
        EXPECTED_EPOCH_1,
    )
    .await;

    check_users_have_same_group_stats(
        &steward,
        &user2,
        GROUP_NAME,
        EXPECTED_MEMBERS_3,
        EXPECTED_EPOCH_1,
    )
    .await;

    check_users_have_same_group_stats(
        &user,
        &user2,
        GROUP_NAME,
        EXPECTED_MEMBERS_3,
        EXPECTED_EPOCH_1,
    )
    .await;
}

#[tokio::test]
async fn test_invite_users_to_group_different_epoch_flow() {
    let (mut steward, mut bob, mut carol) = create_three_test_user_with_group(GROUP_NAME).await;

    let ga_message = get_group_announcement_message(&mut steward, GROUP_NAME).await;
    share_group_announcement_for_one_user(&mut steward, &mut bob, ga_message.clone()).await;

    let steward_epoch_messages =
        steward_epoch_without_user_in_group(&mut steward, GROUP_NAME).await;
    user_join_group(&mut bob, steward_epoch_messages[1].clone()).await;

    check_users_have_same_group_stats(
        &steward,
        &bob,
        GROUP_NAME,
        EXPECTED_MEMBERS_2,
        EXPECTED_EPOCH_1,
    )
    .await;

    println!("START NEW EPOCH");
    println!("--------------------------------");

    let ga_message_2 = get_group_announcement_message(&mut steward, GROUP_NAME).await;
    share_group_announcement_for_one_user(&mut steward, &mut carol, ga_message_2.clone()).await;

    let (steward_epoch_messages_2, _) =
        steward_epoch_with_user_in_group(&mut steward, GROUP_NAME).await;

    println!("Debug: steward vote, wait for user vote");
    println!("--------------------------------");

    let waku_vote_message = user_vote_on_proposal(
        &mut bob,
        steward_epoch_messages_2[0].clone(),
        true,
        GROUP_NAME,
    )
    .await;

    let waku_msgs_to_send = process_and_handle_trigger_event_message(
        &mut steward,
        waku_vote_message,
        GroupState::Working,
    )
    .await;

    bob.process_waku_message(waku_msgs_to_send[0].clone())
        .await
        .expect("Failed to process waku message");

    user_join_group(&mut carol, waku_msgs_to_send[1].clone()).await;

    check_users_have_same_group_stats(
        &steward,
        &bob,
        GROUP_NAME,
        EXPECTED_MEMBERS_3,
        EXPECTED_EPOCH_2,
    )
    .await;

    check_users_have_same_group_stats(
        &steward,
        &carol,
        GROUP_NAME,
        EXPECTED_MEMBERS_3,
        EXPECTED_EPOCH_2,
    )
    .await;

    check_users_have_same_group_stats(
        &bob,
        &carol,
        GROUP_NAME,
        EXPECTED_MEMBERS_3,
        EXPECTED_EPOCH_2,
    )
    .await;
}

// #[tokio::test]
// async fn test_remove_user_flow_request_from_steward() {
//     let (mut steward, mut bob, mut carol) = create_three_test_user_with_group(GROUP_NAME).await;

//     let ga_message = get_group_announcement_message(&mut steward, GROUP_NAME).await;

//     share_group_announcement_for_one_user(&mut steward, &mut bob, ga_message.clone()).await;
//     share_group_announcement_for_one_user(&mut steward, &mut carol, ga_message.clone()).await;

//     let steward_epoch_messages =
//         steward_epoch_without_user_in_group(&mut steward, GROUP_NAME).await;

//     user_join_group(&mut bob, steward_epoch_messages[1].clone()).await;
//     user_join_group(&mut carol, steward_epoch_messages[1].clone()).await;

//     check_users_have_same_group_stats(
//         &steward,
//         &bob,
//         GROUP_NAME,
//         EXPECTED_MEMBERS_3,
//         EXPECTED_EPOCH_1,
//     )
//     .await;

//     check_users_have_same_group_stats(
//         &steward,
//         &carol,
//         GROUP_NAME,
//         EXPECTED_MEMBERS_3,
//         EXPECTED_EPOCH_1,
//     )
//     .await;

//     check_users_have_same_group_stats(
//         &bob,
//         &carol,
//         GROUP_NAME,
//         EXPECTED_MEMBERS_3,
//         EXPECTED_EPOCH_1,
//     )
//     .await;

//     let ban_request_message = create_ban_request_message(&mut steward, GROUP_NAME).await;

//     let action = bob
//         .process_waku_message(ban_request_message.clone())
//         .await
//         .expect("Failed to process ban request");

//     match action {
//         UserAction::SendToApp(_) => {
//             println!("Debug: SendToApp action");
//         }
//         _ => panic!("Expected SendToApp action"),
//     }

//     let action = carol
//         .process_waku_message(ban_request_message.clone())
//         .await
//         .expect("Failed to process ban request");

//     match action {
//         UserAction::SendToApp(_) => {
//             println!("Debug: SendToApp action");
//         }
//         _ => panic!("Expected SendToApp action"),
//     }

//     let (steward_epoch_messages, proposal_id) =
//         steward_epoch_with_user_in_group(&mut steward, GROUP_NAME).await;

//     steward
//         .set_up_consensus_threshold_for_group(GROUP_NAME, proposal_id, 1f64)
//         .await
//         .expect("Can't setup threshold");

//     println!("Debug: Bob vote");
//     let waku_vote_message = user_vote_on_proposal(
//         &mut bob,
//         steward_epoch_messages[0].clone(),
//         true,
//         GROUP_NAME,
//     )
//     .await;

//     println!("Debug: Carol vote");
//     let waku_vote_message_2 = user_vote_on_proposal(
//         &mut carol,
//         steward_epoch_messages[0].clone(),
//         true,
//         GROUP_NAME,
//     )
//     .await;

//     println!("Debug: steward process bob vote");
//     let waku_msgs_to_send = process_and_handle_trigger_event_message(
//         &mut steward,
//         waku_vote_message,
//         GroupState::Voting,
//     )
//     .await;
//     println!("Debug: waku_msgs_to_send after bob vote: {waku_msgs_to_send:?}");

//     println!("Debug: steward process carol vote");
//     let waku_msgs_to_send_2 = process_and_handle_trigger_event_message(
//         &mut steward,
//         waku_vote_message_2,
//         GroupState::Working,
//     )
//     .await;
//     println!("Debug: waku_msgs_to_send_2 after carol vote: {waku_msgs_to_send_2:?}");
// }
