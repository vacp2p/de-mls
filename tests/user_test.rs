use de_mls::{
    protos::messages::v1::app_message,
    user::{User, UserAction},
    ws_actor::{RawWsMessage, UserMessage, WsAction},
};

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

    let group_announcement = alice
        .prepare_steward_msg(group_name.clone())
        .await
        .expect("Failed to prepare steward message");
    let group_announcement_message = group_announcement
        .build_waku_message()
        .expect("Failed to build waku message");

    // Bob parce GA message and share his KP to Alice
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

    // Alice parce Bob's KP and add it to the queue of income key packages
    let _alice_action = alice
        .process_waku_message(bob_kp_waku_message)
        .await
        .expect("Failed to process waku message");

    // Carol parce GA message and share her KP to Alice
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

    // Alice parce Carol's KP and add it to the queue of income key packages
    let _alice_action = alice
        .process_waku_message(carol_kp_waku_message)
        .await
        .expect("Failed to process waku message");

    // Alice start adding both into group
    // 1. Get income key packages from the queue
    let pending_proposals = alice
        .group_drain_pending_proposals(group_name.clone())
        .await
        .expect("Failed to get income key packages");
    // assert_eq!(pending_proposals.len(), 2);

    // 2. Create invite proposal with pending proposals
    let _ = alice
        .process_proposals(group_name.clone(), pending_proposals)
        .await
        .expect("Failed to process proposals");

    let out = alice
        .apply_proposals(group_name.clone())
        .await
        .expect("Failed to apply proposals");

    let join_msg = out[1]
        .build_waku_message()
        .expect("Failed to build waku message");
    let bob_res_action = bob
        .process_waku_message(join_msg.clone())
        .await
        .expect("Failed to process waku message");
    println!("Bob result: {:?}", bob_res_action);
    let bob_res = match bob_res_action {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let bob_res_waku_message = bob_res
        .build_waku_message()
        .expect("Failed to build waku message");

    let res_alice = alice
        .process_waku_message(bob_res_waku_message.clone())
        .await
        .expect("Failed to process waku message");
    println!("Alice result: {:?}", res_alice);
    let res_alice_msg = match res_alice {
        UserAction::SendToApp(msg) => msg,
        _ => panic!("User action is not SendToApp"),
    };

    let inside_msg = match res_alice_msg.payload.unwrap() {
        app_message::Payload::ConversationMessage(msg) => msg,
        _ => panic!("User action is not SendToApp"),
    };
    println!(
        "Alice message: {:?}",
        String::from_utf8(inside_msg.message).unwrap()
    );

    let res_carol = carol
        .process_waku_message(join_msg)
        .await
        .expect("Failed to process waku message");
    println!("Carol result: {:?}", res_carol);

    let carol_group = carol
        .get_group(group_name.clone())
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

    // Bob parce GA message and share his KP to Alice
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

    // Alice parce Bob's KP and add it to the queue of income key packages
    let _alice_action = alice
        .process_waku_message(bob_kp_waku_message)
        .await
        .expect("Failed to process waku message with Bob's KP");

    // Alice start adding Bob into group
    // 1. Get income key packages from the queue
    let pending_proposals = alice
        .group_drain_pending_proposals(group_name.clone())
        .await
        .expect("Failed to get pending proposals while adding Bob to the group");
    // assert_eq!(pending_proposals.len(), 1);

    // 2. Create invite proposal with pending proposals
    let _ = alice
        .process_proposals(group_name.clone(), pending_proposals)
        .await
        .expect("Failed to process proposals while adding Bob to the group");

    // 3. Apply proposals to add Bob to the group
    let out = alice
        .apply_proposals(group_name.clone())
        .await
        .expect("Failed to apply proposals while adding Bob to the group");

    // 4. Bob process join message
    let _ = match bob
        .process_waku_message(
            out[1]
                .build_waku_message()
                .expect("Failed to build waku message after  to the group"),
        )
        .await
        .expect("Failed to process waku message for Bob to join to the group")
    {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("Bob action is not SendToWaku"),
    };

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

    // Carol parce GA message and share her KP to Alice
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

    // Alice parce Carol's KP and add it to the queue of income key packages
    let alice_action = alice
        .process_waku_message(carol_kp_waku_message)
        .await
        .expect("Failed to process waku message with Carol's KP");
    let alice_action_msg = match alice_action {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("Alice action is not SendToWaku"),
    };
    let alice_action_msg_waku = alice_action_msg
        .build_waku_message()
        .expect("Failed to build waku message with Alice's action");

    let bob_action = bob
        .process_waku_message(alice_action_msg_waku.clone())
        .await
        .expect("Failed to process waku message with Alice's action");

    // Alice start adding Carol into group
    // 1. Get income key packages from the queue
    let pending_proposals = alice
        .group_drain_pending_proposals(group_name.clone())
        .await
        .expect("Failed to get pending proposals while adding Bob to the group");
    // assert_eq!(pending_proposals.len(), 1);

    // 2. Create invite proposal with pending proposals
    let _ = alice
        .process_proposals(group_name.clone(), pending_proposals)
        .await
        .expect("Failed to process proposals while adding Bob to the group");

    // 3. Apply proposals to add Bob to the group
    let out = alice
        .apply_proposals(group_name.clone())
        .await
        .expect("Failed to apply proposals while adding Bob to the group");

    // 4. Carol process join message
    let _ = match carol
        .process_waku_message(
            out[1]
                .build_waku_message()
                .expect("Failed to build waku message for Carol to join to the group"),
        )
        .await
        .expect("Failed to process waku message for Carol to join to the group")
    {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("Carol action is not SendToWaku"),
    };

    // 5. Bob process commit message
    match bob
        .process_waku_message(
            out[0]
                .build_waku_message()
                .expect("Failed to build waku message apply commit to the Bob"),
        )
        .await
        .expect("Failed to process waku message apply commit to the Bob")
    {
        UserAction::SendToWaku(msg) => {
            println!("Bob action is SendToWaku: {:?}", msg);
        }
        UserAction::DoNothing => {
            println!("Bob action is DoNothing");
        }
        _ => panic!("Bob action is not SendToWaku"),
    };

    let carol_group = carol
        .get_group(group_name.clone())
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

    // let users_to_invite = alice
    //     .get_processed_income_key_packages(group_name.clone())
    //     .await
    //     .expect("Failed to process income key packages");
    // assert!(users_to_invite.len() == 2, "Expected 2 users to invite");

    // let res = alice
    //     .invite_users(users_to_invite, group_name.clone())
    //     .await
    //     .expect("Failed to invite users");
    // assert!(res.len() == 2, "Expected 2 messages to send");

    // let welcome_message = res[1]
    //     .build_waku_message()
    //     .expect("Failed to build waku message");

    // let _bob_action = bob
    //     .process_waku_message(welcome_message.clone())
    //     .await
    //     .expect("Failed to process waku message");

    // let _carol_action = carol
    //     .process_waku_message(welcome_message.clone())
    //     .await
    //     .expect("Failed to process waku message");

    let carol_group = carol
        .get_group(group_name.clone())
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
            let ws_action = if message.starts_with("/") {
                let mut tokens = message.split_whitespace();
                let ws = match tokens.next() {
                    Some("/ban") => {
                        let user_to_ban = tokens.next().expect("Failed to get user to ban");
                        WsAction::RemoveUser(user_to_ban.to_string(), group_id.clone())
                    }
                    _ => {
                        assert!(false, "Invalid user message");
                        WsAction::DoNothing
                    }
                };
                ws
            } else {
                WsAction::UserMessage(UserMessage { message, group_id })
            };
            ws_action
        }
        Err(_) => {
            assert!(false, "Failed to parse user message");
            WsAction::DoNothing
        }
    };
    assert_eq!(
        ws_action,
        WsAction::RemoveUser(
            "f39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
            group_name.clone()
        )
    );

    let pmt = match ws_action {
        WsAction::RemoveUser(user_to_ban, group_name) => alice
            .remove_group_users(vec![user_to_ban], group_name.clone())
            .await
            .expect("Failed to remove user from group"),
        _ => panic!("User action is not RemoveUser"),
    };

    let waku_commit_message = pmt
        .build_waku_message()
        .expect("Failed to build waku message");

    let _ = carol
        .process_waku_message(waku_commit_message.clone())
        .await
        .expect("Failed to process waku message");
    let carol_group = carol
        .get_group(group_name.clone())
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
    let _ = bob
        .leave_group(group_name.clone())
        .await
        .expect("Failed to leave group");
    assert_eq!(
        bob.if_group_exists(group_name.clone()),
        false,
        "Bob is still in the group"
    );
}
