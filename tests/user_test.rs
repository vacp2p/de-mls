use de_mls::{
    message::UserMessage,
    protos::messages::v1::app_message,
    state_machine::GroupState,
    user::{User, UserAction},
    ws_actor::{RawWsMessage, WsAction},
};
use log::info;

#[tokio::test]
async fn test_invite_users_flow() {
    let group_name = "new_group".to_string();

    let alice_priv_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let mut alice = User::new(alice_priv_key).expect("Failed to create user");
    alice
        .create_group(group_name.clone(), true)
        .await
        .expect("Failed to create group");

    let bob_priv_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let mut bob = User::new(bob_priv_key).expect("Failed to create user");
    bob.create_group(group_name.clone(), false)
        .await
        .expect("Failed to create group");

    let carol_priv_key = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
    let mut carol = User::new(carol_priv_key).expect("Failed to create user");
    carol
        .create_group(group_name.clone(), false)
        .await
        .expect("Failed to create group");

    let group_announcement_message = alice
        .prepare_steward_msg(group_name.clone())
        .await
        .expect("Failed to prepare steward message")
        .build_waku_message()
        .expect("Failed to build waku message");

    // Bob parse GA message and share his KP to Alice
    let bob_kp_message = match bob
        .process_waku_message(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message")
    {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let bob_kp_waku_message = bob_kp_message
        .build_waku_message()
        .expect("Failed to build waku message");

    // Alice parse Bob's KP and add it to the queue of income key packages
    let _alice_action = alice
        .process_waku_message(bob_kp_waku_message)
        .await
        .expect("Failed to process waku message");

    // Carol parse GA message and share her KP to Alice
    let carol_kp_message = match carol
        .process_waku_message(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message")
    {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let carol_kp_waku_message = carol_kp_message
        .build_waku_message()
        .expect("Failed to build waku message");

    // Alice parse Carol's KP and add it to the queue of income key packages
    let _alice_action = alice
        .process_waku_message(carol_kp_waku_message)
        .await
        .expect("Failed to process waku message");

    // Debug: Check how many proposals we have
    let proposal_count_before = alice
        .get_pending_proposals_count(group_name.clone())
        .await
        .expect("Failed to get proposal count");
    println!("Debug: Proposal count before steward epoch: {proposal_count_before}");

    // Add Bob and Carol to the group initially using steward epoch flow
    // State machine: start steward epoch, voting, complete voting
    let steward_epoch_proposals = alice
        .start_steward_epoch(group_name.clone())
        .await
        .expect("Failed to start steward epoch");

    println!("Debug: Steward epoch returned {steward_epoch_proposals} proposals");

    let (proposal_id, action) = alice
        .start_voting(group_name.clone())
        .await
        .expect("Failed to start voting");

    let _alice_proposal_waku_message = match action {
        UserAction::SendToWaku(waku_msg) => waku_msg,
        _ => panic!("User action is not SendToWaku"),
    };

    let vote_result_messages = alice
        .complete_voting_for_steward(group_name.clone(), proposal_id)
        .await
        .expect("Failed to complete voting");

    // Bob processes the welcome message to join the group
    bob.process_waku_message(
        vote_result_messages[1]
            .build_waku_message()
            .expect("Failed to build waku welcome message for Bob"),
    )
    .await
    .expect("Failed to process waku welcome message for Bob");

    // Bob sends a message after joining
    let bob_res_waku_message = bob
        .build_group_message("User joined to the group", group_name.clone())
        .await
        .expect("Failed to build group message")
        .build_waku_message()
        .expect("Failed to build waku message");

    let res_alice = alice
        .process_waku_message(bob_res_waku_message.clone())
        .await
        .expect("Failed to process waku message");
    println!("Alice result: {res_alice:?}");
    let res_alice_msg = match res_alice {
        UserAction::SendToApp(msg) => msg,
        _ => panic!("User action is not SendToApp"),
    };

    let inside_msg = match res_alice_msg.payload.expect("Payload is none") {
        app_message::Payload::ConversationMessage(msg) => msg,
        _ => panic!("User action is not SendToApp"),
    };
    println!(
        "Alice message: {:?}",
        String::from_utf8(inside_msg.message).expect("Failed to convert message to string")
    );

    // Carol processes the welcome message to join the group
    let res_carol = carol
        .process_waku_message(
            vote_result_messages[1]
                .build_waku_message()
                .expect("Failed to build waku welcome message for Carol"),
        )
        .await
        .expect("Failed to process waku message");
    println!("Carol result: {res_carol:?}");
    let carol_group = carol
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let carol_members = carol_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        carol_members.len() == 3,
        "Wrong number of members in the group for Carol"
    );
    let carol_group_epoch = carol_group.epoch().await.expect("Failed to get epoch");
    assert_eq!(carol_group_epoch.as_u64(), 1, "Carol group epoch is not 1");

    let bob_group = bob
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let bob_members = bob_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        bob_members.len() == 3,
        "Wrong number of members in the group for Bob"
    );
    let bob_group_epoch = bob_group.epoch().await.expect("Failed to get epoch");
    assert_eq!(bob_group_epoch.as_u64(), 1, "Bob group epoch is not 1");

    let alice_group = alice
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let alice_members = alice_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        alice_members.len() == 3,
        "Wrong number of members in the group for Alice"
    );
    let alice_group_epoch = alice_group.epoch().await.expect("Failed to get epoch");
    assert_eq!(alice_group_epoch.as_u64(), 1, "Alice group epoch is not 1");

    assert_eq!(
        alice_members, bob_members,
        "Alice and Bob have different members"
    );
    assert_eq!(
        alice_members, carol_members,
        "Alice and Carol have different members"
    );
    assert_eq!(
        bob_members, carol_members,
        "Bob and Carol have different members"
    );
}

#[tokio::test]
async fn test_add_user_in_different_epoch() {
    let group_name = "new_group".to_string();

    let alice_priv_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let mut alice = User::new(alice_priv_key).expect("Failed to create user");
    alice
        .create_group(group_name.clone(), true)
        .await
        .expect("Failed to create group for Alice");

    let bob_priv_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let mut bob = User::new(bob_priv_key).expect("Failed to create user");
    bob.create_group(group_name.clone(), false)
        .await
        .expect("Failed to create group for Bob");

    // First group announcement to add Bob to the group
    let group_announcement_message = alice
        .prepare_steward_msg(group_name.clone())
        .await
        .expect("Failed to prepare steward message for Bob")
        .build_waku_message()
        .expect("Failed to build waku message with group announcement for Bob");

    // Bob parse GA message and share his KP to Alice
    let bob_kp_message = match bob
        .process_waku_message(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message with group announcement for Bob")
    {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("Bob action is not SendToWaku"),
    };

    let bob_kp_waku_message = bob_kp_message
        .build_waku_message()
        .expect("Failed to build waku message with Bob's KP");

    // Alice parse Bob's KP and add it to the queue of income key packages
    let _alice_action = alice
        .process_waku_message(bob_kp_waku_message)
        .await
        .expect("Failed to process waku message with Bob's KP");

    // Alice start adding Bob into group
    // State machine: start steward epoch, voting, complete voting (Bob)
    println!(
        "Test: Before start_steward_epoch (Bob), group state: {:?}",
        alice
            .get_group(group_name.clone())
            .await
            .expect("Failed to get group")
            .get_state()
            .await
    );
    alice
        .start_steward_epoch(group_name.clone())
        .await
        .expect("Failed to start steward epoch (Bob)");
    println!(
        "Test: After start_steward_epoch (Bob), group state: {:?}",
        alice
            .get_group(group_name.clone())
            .await
            .expect("Failed to get group")
            .get_state()
            .await
    );

    let (proposal_id, action) = alice
        .start_voting(group_name.clone())
        .await
        .expect("Failed to start voting");

    let _alice_proposal_waku_message = match action {
        UserAction::SendToWaku(waku_msg) => waku_msg,
        _ => panic!("User action is not SendToWaku"),
    };

    let vote_result_messages = alice
        .complete_voting_for_steward(group_name.clone(), proposal_id)
        .await
        .expect("Failed to complete voting");

    // Bob processes the welcome message to join the group
    bob.process_waku_message(
        vote_result_messages[1]
            .build_waku_message()
            .expect("Failed to build waku welcome message for Bob"),
    )
    .await
    .expect("Failed to process waku welcome message for Bob");

    // Bob sends a message after joining
    let bob_res_waku_message = bob
        .build_group_message("User joined to the group", group_name.clone())
        .await
        .expect("Failed to build group message")
        .build_waku_message()
        .expect("Failed to build waku message");

    let res_alice = alice
        .process_waku_message(bob_res_waku_message.clone())
        .await
        .expect("Failed to process waku message");
    println!("Alice result: {res_alice:?}");
    let res_alice_msg = match res_alice {
        UserAction::SendToApp(msg) => msg,
        _ => panic!("User action is not SendToApp"),
    };

    let inside_msg = match res_alice_msg.payload.expect("Payload is none") {
        app_message::Payload::ConversationMessage(msg) => msg,
        _ => panic!("User action is not SendToApp"),
    };
    println!(
        "Alice message: {:?}",
        String::from_utf8(inside_msg.message).expect("Failed to convert message to string")
    );

    // Adding Carol to the group in different epoch
    let carol_priv_key = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
    let mut carol = User::new(carol_priv_key).expect("Failed to create user");
    carol
        .create_group(group_name.clone(), false)
        .await
        .expect("Failed to create group");

    // Second group announcement to add Carol to the group
    let group_announcement_message_2 = alice
        .prepare_steward_msg(group_name.clone())
        .await
        .expect("Failed to prepare steward message for Carol")
        .build_waku_message()
        .expect("Failed to build waku message with group announcement for Carol");

    // Carol parse GA message and share her KP to Alice
    let carol_kp_message = match carol
        .process_waku_message(group_announcement_message_2.clone())
        .await
        .expect("Failed to process waku message with group announcement for Carol")
    {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("Carol action is not SendToWaku"),
    };

    let carol_kp_waku_message = carol_kp_message
        .build_waku_message()
        .expect("Failed to build waku message with Carol's KP");

    // Alice parse Carol's KP and add it to the queue of income key packages
    alice
        .process_waku_message(carol_kp_waku_message)
        .await
        .expect("Failed to process waku message with Carol's KP");

    // Alice start adding Carol into group

    // State machine: start steward epoch, voting, complete voting (Carol)
    println!(
        "Test: Before start_steward_epoch (Carol), group state: {:?}",
        alice
            .get_group(group_name.clone())
            .await
            .expect("Failed to get group")
            .get_state()
            .await
    );
    alice
        .start_steward_epoch(group_name.clone())
        .await
        .expect("Failed to start steward epoch (Carol)");
    println!(
        "Test: After start_steward_epoch (Carol), group state: {:?}",
        alice
            .get_group(group_name.clone())
            .await
            .expect("Failed to get group")
            .get_state()
            .await
    );

    let (proposal_id, action) = alice
        .start_voting(group_name.clone())
        .await
        .expect("Failed to start voting (Carol)");
    println!(
        "Test: After start_voting (Carol), group state: {:?}",
        alice
            .get_group(group_name.clone())
            .await
            .expect("Failed to get group")
            .get_state()
            .await
    );

    let alice_proposal_waku_message = match action {
        UserAction::SendToWaku(waku_msg) => waku_msg,
        _ => panic!("User action is not SendToWaku"),
    };

    // Submit a vote (Alice votes yes for her own proposals)
    println!("Test: Submitting vote with ID: {proposal_id:?}");
    println!("Test: Alice's identity: {}", alice.identity_string());
    let bob_vote_message = bob
        .process_waku_message(
            alice_proposal_waku_message
                .clone()
                .build_waku_message()
                .expect("Failed to build waku message"),
        )
        .await
        .expect("Failed to process waku message");

    let bob_proposal_to_vote = match bob_vote_message {
        UserAction::SendToApp(msg) => msg,
        _ => panic!("User action is not SendToAPP"),
    };
    println!("Bob will vote: {bob_proposal_to_vote:?}");
    let bob_vote_waku_message = match bob
        .process_user_vote(proposal_id, true, group_name.clone())
        .await
        .expect("Can't get waku msg")
    {
        UserAction::SendToWaku(wmts) => wmts,
        _ => panic!("User action is not SendToWaku"),
    };

    alice
        .process_waku_message(
            bob_vote_waku_message
                .clone()
                .build_waku_message()
                .expect("Failed to build waku message"),
        )
        .await
        .expect("Failed to process waku message");

    println!(
        "Test: Before complete_voting (Carol), group state: {:?}",
        alice
            .get_group(group_name.clone())
            .await
            .expect("Failed to get group")
            .get_state()
            .await
    );
    let vote_result_messages_2 = alice
        .complete_voting_for_steward(group_name.clone(), proposal_id)
        .await
        .expect("Failed to complete voting (Carol)");
    println!(
        "Test: After complete_voting (Carol), group state: {:?}",
        alice
            .get_group(group_name.clone())
            .await
            .expect("Failed to get group")
            .get_state()
            .await
    );

    // Carol process join message
    carol
        .process_waku_message(
            vote_result_messages_2[1]
                .build_waku_message()
                .expect("Failed to build waku message for Carol to join to the group"),
        )
        .await
        .expect("Failed to process waku message for Carol to join to the group");

    // 5. Bob process commit message
    match bob
        .process_waku_message(
            vote_result_messages_2[0]
                .build_waku_message()
                .expect("Failed to build waku message apply commit to the Bob"),
        )
        .await
        .expect("Failed to process waku message apply commit to the Bob")
    {
        UserAction::SendToWaku(msg) => {
            println!("Bob action is SendToWaku: {msg:?}");
        }
        UserAction::DoNothing => {
            println!("Bob action is DoNothing");
        }
        _ => panic!("Bob action is not SendToWaku"),
    };

    let carol_group = carol
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let carol_members = carol_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        carol_members.len() == 3,
        "Wrong number of members in the group for Carol"
    );
    let carol_group_epoch = carol_group.epoch().await.expect("Failed to get epoch");
    assert_eq!(carol_group_epoch.as_u64(), 2, "Carol group epoch is not 2");

    let bob_group = bob
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let bob_members = bob_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        bob_members.len() == 3,
        "Wrong number of members in the group for Bob"
    );
    let bob_group_epoch = bob_group.epoch().await.expect("Failed to get epoch");
    assert_eq!(bob_group_epoch.as_u64(), 2, "Bob group epoch is not 2");

    let alice_group = alice
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let alice_members = alice_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        alice_members.len() == 3,
        "Wrong number of members in the group for Alice"
    );
    let alice_group_epoch = alice_group.epoch().await.expect("Failed to get epoch");
    assert_eq!(alice_group_epoch.as_u64(), 2, "Alice group epoch is not 2");

    assert_eq!(
        alice_members, bob_members,
        "Alice and Bob have different members"
    );
    assert_eq!(
        alice_members, carol_members,
        "Alice and Carol have different members"
    );
    assert_eq!(
        bob_members, carol_members,
        "Bob and Carol have different members"
    );
}

#[tokio::test]
async fn test_remove_user_flow() {
    let group_name = "new_group".to_string();

    let alice_priv_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let mut alice = User::new(alice_priv_key).expect("Failed to create user");
    alice
        .create_group(group_name.clone(), true)
        .await
        .expect("Failed to create group");

    let bob_priv_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let mut bob = User::new(bob_priv_key).expect("Failed to create user");
    bob.create_group(group_name.clone(), false)
        .await
        .expect("Failed to create group");

    let carol_priv_key = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
    let mut carol = User::new(carol_priv_key).expect("Failed to create user");
    carol
        .create_group(group_name.clone(), false)
        .await
        .expect("Failed to create group");

    let group_announcement = alice
        .prepare_steward_msg(group_name.clone())
        .await
        .expect("Failed to prepare steward message");
    let group_announcement_message = group_announcement
        .build_waku_message()
        .expect("Failed to build waku message");

    let bob_action = bob
        .process_waku_message(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message");
    let bob_kp_message = match bob_action {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let bob_kp_waku_message = bob_kp_message
        .build_waku_message()
        .expect("Failed to build waku message");

    let _alice_action = alice
        .process_waku_message(bob_kp_waku_message)
        .await
        .expect("Failed to process waku message");

    let carol_action = carol
        .process_waku_message(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message");
    let carol_kp_message = match carol_action {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let carol_kp_waku_message = carol_kp_message
        .build_waku_message()
        .expect("Failed to build waku message");

    let _alice_action = alice
        .process_waku_message(carol_kp_waku_message)
        .await
        .expect("Failed to process waku message");

    // Debug: Check how many proposals we have
    let proposal_count_before = alice
        .get_pending_proposals_count(group_name.clone())
        .await
        .expect("Failed to get proposal count");
    println!("Debug: Proposal count before steward epoch: {proposal_count_before}");

    // Add Bob and Carol to the group initially using steward epoch flow
    // State machine: start steward epoch, voting, complete voting
    let steward_epoch_proposals = alice
        .start_steward_epoch(group_name.clone())
        .await
        .expect("Failed to start steward epoch");

    println!("Debug: Steward epoch returned {steward_epoch_proposals} proposals");

    let (proposal_id, _action) = alice
        .start_voting(group_name.clone())
        .await
        .expect("Failed to start voting");

    // Submit a vote (Alice votes yes for her own proposals)
    let vote_result_messages = alice
        .complete_voting_for_steward(group_name.clone(), proposal_id)
        .await
        .expect("Failed to complete voting");

    // Bob processes the welcome message to join the group
    bob.process_waku_message(
        vote_result_messages[1]
            .build_waku_message()
            .expect("Failed to build waku welcome message for Bob"),
    )
    .await
    .expect("Failed to process waku welcome message for Bob");

    // Bob sends a message after joining
    let bob_res_waku_message = bob
        .build_group_message("User joined to the group", group_name.clone())
        .await
        .expect("Failed to build group message")
        .build_waku_message()
        .expect("Failed to build waku message");

    let res_alice = alice
        .process_waku_message(bob_res_waku_message.clone())
        .await
        .expect("Failed to process waku message");
    println!("Alice result: {res_alice:?}");
    let res_alice_msg = match res_alice {
        UserAction::SendToApp(msg) => msg,
        _ => panic!("User action is not SendToApp"),
    };

    let inside_msg = match res_alice_msg.payload.expect("Payload is none") {
        app_message::Payload::ConversationMessage(msg) => msg,
        _ => panic!("User action is not SendToApp"),
    };
    println!(
        "Alice message: {:?}",
        String::from_utf8(inside_msg.message).expect("Failed to convert message to string")
    );

    // Carol processes the welcome message to join the group
    let res_carol = carol
        .process_waku_message(
            vote_result_messages[1]
                .build_waku_message()
                .expect("Failed to build waku welcome message for Carol"),
        )
        .await
        .expect("Failed to process waku message");
    println!("Carol result: {res_carol:?}");

    let carol_group = carol
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let carol_members = carol_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        carol_members.len() == 3,
        "Wrong number of members in the group for Carol"
    );
    let bob_group = bob
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let bob_members = bob_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        bob_members.len() == 3,
        "Wrong number of members in the group for Bob"
    );
    let alice_group = alice
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let alice_members = alice_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        alice_members.len() == 3,
        "Wrong number of members in the group for Alice"
    );

    assert_eq!(
        alice_members, bob_members,
        "Alice and Bob have different members"
    );
    assert_eq!(
        alice_members, carol_members,
        "Alice and Carol have different members"
    );
    assert_eq!(
        bob_members, carol_members,
        "Bob and Carol have different members"
    );

    let raw_msg = RawWsMessage {
        message: serde_json::to_string(&UserMessage {
            message: "/ban f39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
            group_id: group_name.clone(),
        })
        .expect("Failed to serialize user message"),
    };

    let ws_action = match serde_json::from_str(&raw_msg.message) {
        Ok(UserMessage { message, group_id }) => {
            if message.starts_with("/") {
                let mut tokens = message.split_whitespace();
                match tokens.next() {
                    Some("/ban") => {
                        let user_to_ban = tokens.next().expect("Failed to get user to ban");
                        WsAction::RemoveUser(user_to_ban.to_string(), group_id.clone())
                    }
                    _ => {
                        panic!("Invalid user message");
                    }
                }
            } else {
                WsAction::UserMessage(UserMessage { message, group_id })
            }
        }
        Err(_) => {
            panic!("Failed to parse user message");
        }
    };
    assert_eq!(
        ws_action,
        WsAction::RemoveUser(
            "f39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
            group_name.clone()
        )
    );

    match ws_action {
        WsAction::RemoveUser(user_to_ban, group_name) => {
            // Add remove proposal to steward instead of direct removal
            alice
                .add_remove_proposal(group_name.clone(), user_to_ban.clone())
                .await
                .expect("Failed to add remove proposal to steward");
        }
        _ => panic!("User action is not RemoveUser"),
    };

    // State machine: start steward epoch, voting, complete voting (removal)
    alice
        .start_steward_epoch(group_name.clone())
        .await
        .expect("Failed to start steward epoch (removal)");
    let (proposal_id, action) = alice
        .start_voting(group_name.clone())
        .await
        .expect("Failed to start voting (removal)");

    let alice_proposal_waku_message = match action {
        UserAction::SendToWaku(waku_msg) => waku_msg,
        _ => panic!("User action is not SendToWaku"),
    };

    // Submit a vote (Alice votes yes for the removal)
    println!("Test: Submitting vote with ID: {proposal_id:?}");
    println!("Test: Alice's identity: {}", alice.identity_string());
    let bob_vote_message = bob
        .process_waku_message(
            alice_proposal_waku_message
                .clone()
                .build_waku_message()
                .expect("Failed to build waku message"),
        )
        .await
        .expect("Failed to process waku message");

    let bob_proposal_to_vote = match bob_vote_message {
        UserAction::SendToApp(msg) => msg,
        _ => panic!("User action is not SendToAPP"),
    };
    println!("Bob will vote: {bob_proposal_to_vote:?}");
    let bob_vote_waku_message = match bob
        .process_user_vote(proposal_id, true, group_name.clone())
        .await
        .expect("Can't get waku msg")
    {
        UserAction::SendToWaku(wmts) => wmts,
        _ => panic!("User action is not SendToWaku"),
    };

    alice
        .process_waku_message(
            bob_vote_waku_message
                .clone()
                .build_waku_message()
                .expect("Failed to build waku message"),
        )
        .await
        .expect("Failed to process waku message");

    let carol_vote_message = carol
        .process_waku_message(
            alice_proposal_waku_message
                .clone()
                .build_waku_message()
                .expect("Failed to build waku message"),
        )
        .await
        .expect("Failed to process waku message");

    let carol_proposal_to_vote = match carol_vote_message {
        UserAction::SendToApp(msg) => msg,
        _ => panic!("User action is not SendToAPP"),
    };
    println!("Carol will vote: {carol_proposal_to_vote:?}");
    let carol_vote_waku_message = match carol
        .process_user_vote(proposal_id, true, group_name.clone())
        .await
        .expect("Can't get waku msg")
    {
        UserAction::SendToWaku(wmts) => wmts,
        _ => panic!("User action is not SendToWaku"),
    };

    alice
        .process_waku_message(
            carol_vote_waku_message
                .clone()
                .build_waku_message()
                .expect("Failed to build waku message"),
        )
        .await
        .expect("Failed to process waku message");

    let vote_result_messages = alice
        .complete_voting_for_steward(group_name.clone(), proposal_id)
        .await
        .expect("Failed to complete voting (removal)");

    let waku_commit_message = vote_result_messages[0]
        .build_waku_message()
        .expect("Failed to build waku message");

    let _ = carol
        .process_waku_message(waku_commit_message.clone())
        .await
        .expect("Failed to process waku message");
    let carol_group = carol
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    let carol_members = carol_group
        .members_identity()
        .await
        .expect("Failed to get members");
    assert!(
        carol_members.len() == 2,
        "Bob is not removed from the group"
    );

    let bob_action = bob
        .process_waku_message(waku_commit_message.clone())
        .await
        .expect("Failed to process waku message");
    assert_eq!(
        bob_action,
        UserAction::LeaveGroup(group_name.clone()),
        "User action is not RemoveGroup"
    );
    bob.leave_group(group_name.clone())
        .await
        .expect("Failed to leave group");
    assert!(
        !bob.if_group_exists(group_name.clone()).await,
        "Bob is still in the group"
    );
}

#[tokio::test]
async fn test_steward_epoch_with_no_proposals() {
    let group_name = "test_steward_no_proposals".to_string();

    // Create steward group
    let alice_priv_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let mut alice = User::new(alice_priv_key).expect("Failed to create user");
    alice
        .create_group(group_name.clone(), true)
        .await
        .expect("Failed to create group");

    // Start steward epoch (should return 0 when no proposals)
    let proposal_count = alice
        .start_steward_epoch(group_name.clone())
        .await
        .expect("Failed to start steward epoch");

    // Should return 0 when no proposals
    assert_eq!(proposal_count, 0);

    // Check that group is still in Working state (no steward epoch started)
    let group = alice
        .get_group(group_name.clone())
        .await
        .expect("Failed to get group");
    assert_eq!(group.get_state().await, GroupState::Working);

    // Since no steward epoch was started, we can't start voting
    let start_vote_result = alice.start_voting(group_name.clone()).await;
    assert!(start_vote_result.is_ok());

    let (proposal_id, action) = start_vote_result.expect("Failed to start voting");
    assert_eq!(proposal_id, 0);
    assert_eq!(action, UserAction::DoNothing);

    info!("Steward epoch correctly skipped when no proposals exist");
}
