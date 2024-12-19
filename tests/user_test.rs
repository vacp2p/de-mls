use de_mls::user::{User, UserAction};
use ds::waku_actor::build_waku_message;

#[tokio::test]
async fn test_admin_message_flow() {
    env_logger::init();
    let group_name = "new_group".to_string();
    let app_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    let alice_priv_key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let mut alice = User::new(alice_priv_key).unwrap();
    alice.create_group(group_name.clone(), true).await.unwrap();

    let alice_group = alice.get_group(group_name.clone()).unwrap();
    assert_eq!(
        alice_group.is_mls_group_initialized(),
        true,
        "MLS group is notinitialized"
    );

    let bob_priv_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let mut bob = User::new(bob_priv_key).unwrap();
    bob.create_group(group_name.clone(), false).await.unwrap();

    let bob_group = bob.get_group(group_name.clone()).unwrap();
    assert_eq!(
        bob_group.is_mls_group_initialized(),
        false,
        "MLS group is initialized"
    );

    // Alice send Group Announcement msg to Bob
    let res = alice.prepare_admin_msg(group_name.clone()).await;
    assert!(res.is_ok(), "Failed to prepare admin message");
    let alice_ga_msg = res.unwrap();

    let res = build_waku_message(alice_ga_msg.clone(), app_id.clone());
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
    let res = build_waku_message(bob_kp_message.clone(), app_id.clone());
    assert!(res.is_ok(), "Failed to build waku message");
    let waku_kp_message = res.unwrap();

    // Alice receives the Key Package Share msg and send Welcome msg to Bob
    let res = alice.process_waku_msg(waku_kp_message).await;
    assert!(res.is_ok(), "Failed to process waku message");
    let user_action = res.unwrap();
    assert!(user_action.len() == 2, "User action is not a two actions");

    let alice_welcome_message = match user_action[1].clone() {
        UserAction::SendToWaku(msg) => msg,
        _ => panic!("User action is not SendToWaku"),
    };
    let res = build_waku_message(alice_welcome_message.clone(), app_id.clone());
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
}
