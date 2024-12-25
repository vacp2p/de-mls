use de_mls::{
    user::{User, UserAction},
    ws_actor::{RawWsMessage, UserMessage, WsAction},
};

#[tokio::test]
async fn test_admin_message_flow() {
    let group_name = "new_group".to_string();

    let alice_priv_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let res = User::new(alice_priv_key);
    assert!(res.is_ok(), "Failed to create user");
    let mut alice = res.unwrap();
    assert!(
        alice.create_group(group_name.clone(), true).await.is_ok(),
        "Failed to create group"
    );

    let res = alice.get_group(group_name.clone());
    assert!(res.is_ok(), "Failed to get group");
    let alice_group = res.unwrap();
    assert_eq!(
        alice_group.is_mls_group_initialized(),
        true,
        "MLS group is notinitialized"
    );

    let bob_priv_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let res = User::new(bob_priv_key);
    assert!(res.is_ok(), "Failed to create user");
    let mut bob = res.unwrap();
    assert!(
        bob.create_group(group_name.clone(), false).await.is_ok(),
        "Failed to create group"
    );

    let res = bob.get_group(group_name.clone());
    assert!(res.is_ok(), "Failed to get group");
    let bob_group = res.unwrap();
    assert_eq!(
        bob_group.is_mls_group_initialized(),
        false,
        "MLS group is initialized"
    );

    let _ = join_group_flow(&mut alice, &mut bob, group_name.clone()).await;
}

async fn join_group_flow(alice: &mut User, bob: &mut User, group_name: String) -> UserAction {
    // Alice send Group Announcement msg to Bob
    let res = alice.prepare_admin_msg(group_name.clone()).await;
    assert!(res.is_ok(), "Failed to prepare admin message");
    let alice_ga_msg = res.unwrap();

    let res = alice_ga_msg.build_waku_message();
    assert!(res.is_ok(), "Failed to build waku message");
    let waku_ga_message = res.unwrap();

    // Bob receives the Group Announcement msg and send Key Package Share msg to Alice
    let res = bob.process_waku_msg(waku_ga_message).await;
    assert!(res.is_ok(), "Failed to process waku message");
    let user_action = res.unwrap();
    assert!(user_action.len() == 1, "User action is not a single action");

    let bob_kp_message = match user_action[0].clone() {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let res = bob_kp_message.build_waku_message();
    assert!(res.is_ok(), "Failed to build waku message");
    let waku_kp_message = res.unwrap();

    // Alice receives the Key Package Share msg and send Welcome msg to Bob
    let res = alice.process_waku_msg(waku_kp_message).await;
    assert!(res.is_ok(), "Failed to process waku message");
    let user_action_invite = res.unwrap();
    assert!(
        user_action_invite.len() == 2,
        "User action is not a two actions"
    );

    let alice_welcome_message = match user_action_invite[1].clone() {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let res = alice_welcome_message.build_waku_message();
    assert!(res.is_ok(), "Failed to build waku message");
    let waku_welcome_message = res.unwrap();

    // Bob receives the Welcome msg and join the group
    let res = bob.process_waku_msg(waku_welcome_message).await;
    assert!(res.is_ok(), "Failed to process waku message");
    let user_action = res.unwrap();
    assert!(user_action.len() == 1, "User action is not a single action");
    let bob_group = bob.get_group(group_name.clone()).unwrap();
    assert!(
        bob_group.is_mls_group_initialized(),
        "MLS group is not initialized"
    );

    user_action_invite[0].clone()
}

#[tokio::test]
async fn test_remove_user_flow() {
    let group_name = "new_group".to_string();

    let alice_priv_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let mut alice = User::new(alice_priv_key).unwrap();
    alice.create_group(group_name.clone(), true).await.unwrap();

    let bob_priv_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let mut bob = User::new(bob_priv_key).unwrap();
    bob.create_group(group_name.clone(), false).await.unwrap();

    let carol_priv_key = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
    let mut carol = User::new(carol_priv_key).unwrap();
    carol.create_group(group_name.clone(), false).await.unwrap();

    let _ = join_group_flow(&mut alice, &mut bob, group_name.clone()).await;
    let res = bob.get_group(group_name.clone());
    assert!(res.is_ok(), "Failed to get group");
    let bob_group = res.unwrap();
    assert!(
        bob_group.is_mls_group_initialized(),
        "MLS group is not initialized"
    );

    let commit_action = join_group_flow(&mut alice, &mut carol, group_name.clone()).await;
    let res = carol.get_group(group_name.clone());
    assert!(res.is_ok(), "Failed to get group");
    let carol_group = res.unwrap();
    assert!(
        carol_group.is_mls_group_initialized(),
        "MLS group is not initialized"
    );
    let pmt = match commit_action {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let commit_message = pmt.build_waku_message();
    assert!(commit_message.is_ok(), "Failed to build waku message");
    let waku_commit_message = commit_message.unwrap();

    let res = bob.process_waku_msg(waku_commit_message.clone()).await;
    assert!(res.is_ok(), "Failed to process waku message");

    let raw_msg = RawWsMessage {
        message: serde_json::to_string(&UserMessage {
            message: "/ban f39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
            group_id: group_name.clone(),
        })
        .unwrap(),
    };

    let ws_action = match serde_json::from_str(&raw_msg.message) {
        Ok(UserMessage { message, group_id }) => {
            let ws_action = if message.starts_with("/") {
                let mut tokens = message.split_whitespace();
                let ws = match tokens.next() {
                    Some("/ban") => {
                        let user_to_ban = tokens.next().unwrap();
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
        WsAction::RemoveUser(user_to_ban, group_name) => {
            let res = alice
                .remove_users_from_group(vec![user_to_ban], group_name.clone())
                .await;
            assert!(res.is_ok(), "Failed to remove user from group");
            res.unwrap()
        }
        _ => panic!("User action is not RemoveUser"),
    };

    let commit_message = pmt.build_waku_message();
    assert!(commit_message.is_ok(), "Failed to build waku message");
    let waku_commit_message = commit_message.unwrap();

    let res = carol.process_waku_msg(waku_commit_message.clone()).await;
    assert!(res.is_ok(), "Failed to process waku message");
    let carol_group = carol.get_group(group_name.clone()).unwrap();
    assert!(
        carol_group.members_identity().await.len() == 2,
        "Bob is not removed from the group"
    );

    let res = bob.process_waku_msg(waku_commit_message.clone()).await;
    assert!(res.is_ok(), "Failed to process waku message");
    let user_action = res.unwrap();
    assert!(user_action.len() == 1, "User action is not a single action");
    assert_eq!(
        user_action[0].clone(),
        UserAction::RemoveGroup(group_name.clone()),
        "User action is not RemoveGroup"
    );
    let res = bob.leave_group(group_name.clone()).await;
    assert!(res.is_ok(), "Failed to leave group");
    assert_eq!(bob.if_group_exists(group_name.clone()), false);
}
