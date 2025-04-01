use de_mls::{
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
        .prepare_admin_msg(group_name.clone())
        .await
        .expect("Failed to prepare admin message");
    let group_announcement_message = group_announcement
        .build_waku_message()
        .expect("Failed to build waku message");

    let bob_action = bob
        .process_waku_msg(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(bob_action.len() == 1, "User action is not a single action");
    let bob_kp_message = match bob_action[0].clone() {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let bob_kp_waku_message = bob_kp_message
        .build_waku_message()
        .expect("Failed to build waku message");

    let alice_action = alice
        .process_waku_msg(bob_kp_waku_message)
        .await
        .expect("Failed to process waku message");
    assert!(
        alice_action.len() == 1,
        "User action is not a single action"
    );

    let carol_action = carol
        .process_waku_msg(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(
        carol_action.len() == 1,
        "User action is not a single action"
    );
    let carol_kp_message = match carol_action[0].clone() {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let carol_kp_waku_message = carol_kp_message
        .build_waku_message()
        .expect("Failed to build waku message");

    let alice_action = alice
        .process_waku_msg(carol_kp_waku_message)
        .await
        .expect("Failed to process waku message");
    assert!(
        alice_action.len() == 1,
        "User action is not a single action"
    );

    let users_to_invite = alice
        .processed_group_income_key_packages(group_name.clone())
        .await
        .expect("Failed to process income key packages");
    assert!(users_to_invite.len() == 2, "Expected 2 users to invite");

    let res = alice
        .invite_users(users_to_invite, group_name.clone())
        .await
        .expect("Failed to invite users");
    assert!(res.len() == 2, "Expected 2 messages to send");

    let welcome_message = res[1]
        .build_waku_message()
        .expect("Failed to build waku message");

    let bob_action = bob
        .process_waku_msg(welcome_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(bob_action.len() == 1, "User action is not a single action");

    let carol_action = carol
        .process_waku_msg(welcome_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(
        carol_action.len() == 1,
        "User action is not a single action"
    );

    let carol_group = carol
        .get_group(group_name.clone())
        .expect("Failed to get group");
    let carol_members = carol_group.members_identity().await;
    assert!(
        carol_members.len() == 3,
        "Wrong number of members in the group for Carol"
    );
    let bob_group = bob
        .get_group(group_name.clone())
        .expect("Failed to get group");
    let bob_members = bob_group.members_identity().await;
    assert!(
        bob_members.len() == 3,
        "Wrong number of members in the group for Bob"
    );
    let alice_group = alice
        .get_group(group_name.clone())
        .expect("Failed to get group");
    let alice_members = alice_group.members_identity().await;
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
        .prepare_admin_msg(group_name.clone())
        .await
        .expect("Failed to prepare admin message");
    let group_announcement_message = group_announcement
        .build_waku_message()
        .expect("Failed to build waku message");

    let bob_action = bob
        .process_waku_msg(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(bob_action.len() == 1, "User action is not a single action");
    let bob_kp_message = match bob_action[0].clone() {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let bob_kp_waku_message = bob_kp_message
        .build_waku_message()
        .expect("Failed to build waku message");

    let alice_action = alice
        .process_waku_msg(bob_kp_waku_message)
        .await
        .expect("Failed to process waku message");
    assert!(
        alice_action.len() == 1,
        "User action is not a single action"
    );

    let carol_action = carol
        .process_waku_msg(group_announcement_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(
        carol_action.len() == 1,
        "User action is not a single action"
    );
    let carol_kp_message = match carol_action[0].clone() {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let carol_kp_waku_message = carol_kp_message
        .build_waku_message()
        .expect("Failed to build waku message");

    let alice_action = alice
        .process_waku_msg(carol_kp_waku_message)
        .await
        .expect("Failed to process waku message");
    assert!(
        alice_action.len() == 1,
        "User action is not a single action"
    );

    let users_to_invite = alice
        .processed_group_income_key_packages(group_name.clone())
        .await
        .expect("Failed to process income key packages");
    assert!(users_to_invite.len() == 2, "Expected 2 users to invite");

    let res = alice
        .invite_users(users_to_invite, group_name.clone())
        .await
        .expect("Failed to invite users");
    assert!(res.len() == 2, "Expected 2 messages to send");

    let welcome_message = res[1]
        .build_waku_message()
        .expect("Failed to build waku message");

    let bob_action = bob
        .process_waku_msg(welcome_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(bob_action.len() == 1, "User action is not a single action");

    let carol_action = carol
        .process_waku_msg(welcome_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(
        carol_action.len() == 1,
        "User action is not a single action"
    );

    let carol_group = carol
        .get_group(group_name.clone())
        .expect("Failed to get group");
    let carol_members = carol_group.members_identity().await;
    assert!(
        carol_members.len() == 3,
        "Wrong number of members in the group for Carol"
    );
    let bob_group = bob
        .get_group(group_name.clone())
        .expect("Failed to get group");
    let bob_members = bob_group.members_identity().await;
    assert!(
        bob_members.len() == 3,
        "Wrong number of members in the group for Bob"
    );
    let alice_group = alice
        .get_group(group_name.clone())
        .expect("Failed to get group");
    let alice_members = alice_group.members_identity().await;
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
            .remove_users_from_group(vec![user_to_ban], group_name.clone())
            .await
            .expect("Failed to remove user from group"),
        _ => panic!("User action is not RemoveUser"),
    };

    let waku_commit_message = pmt
        .build_waku_message()
        .expect("Failed to build waku message");

    let _ = carol
        .process_waku_msg(waku_commit_message.clone())
        .await
        .expect("Failed to process waku message");
    let carol_group = carol
        .get_group(group_name.clone())
        .expect("Failed to get group");
    let carol_members = carol_group.members_identity().await;
    assert!(
        carol_members.len() == 2,
        "Bob is not removed from the group"
    );

    let bob_action = bob
        .process_waku_msg(waku_commit_message.clone())
        .await
        .expect("Failed to process waku message");
    assert!(bob_action.len() == 1, "User action is not a single action");
    assert_eq!(
        bob_action[0].clone(),
        UserAction::RemoveGroup(group_name.clone()),
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
