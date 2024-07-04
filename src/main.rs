use alloy::{primitives::Address, providers::ProviderBuilder};
use bus::Bus;
use std::str::FromStr;
use url::Url;

use openmls::framing::{MlsMessageIn, MlsMessageInBody};

use de_mls::{get_contract_address, user::User};
use sc_key_store::sc_ks::*;

#[tokio::main]
async fn main() {
    let storage_address = Address::from_str("0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512").unwrap();
    let (alice_address, alice_wallet) = alice_addr_test();
    let alice_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(alice_wallet)
        .on_http(Url::from_str("http://localhost:8545").unwrap());

    let (bob_address, bob_wallet) = bob_addr_test();
    let bob_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(bob_wallet)
        .on_http(Url::from_str("http://localhost:8545").unwrap());

    // This channel for message before adding to group.
    // Message are still encrypted, but this channel not attached to any group
    let mut m: Bus<MlsMessageIn> = Bus::new(10);
    let mut a_r = m.add_rx();
    let mut b_r = m.add_rx();

    //// Create user Alice
    println!("Start Register Alice");
    let res = User::new(
        alice_address.as_slice(),
        alice_provider.clone(),
        storage_address,
    )
    .await;
    assert!(res.is_ok());
    let mut a_user = res.unwrap();
    let res = a_user.register().await;
    assert!(res.is_ok());
    println!("Register Alice successfully");
    //////

    //// Create user Bob
    println!("Start Register Bob");
    let res = User::new(
        bob_address.as_slice(),
        bob_provider.clone(),
        storage_address,
    )
    .await;
    assert!(res.is_ok());
    let mut b_user = res.unwrap();
    let res = b_user.register().await;
    assert!(res.is_ok());
    println!("Register Bob successfully");
    //////

    //// Alice create group: Alice_Group
    println!("Start create group");
    let group_name = String::from_str("Alice_Group").unwrap();
    let res = a_user.create_group(group_name.clone()).await;
    assert!(res.is_ok());
    println!("Create group successfully");
    //////

    //// Alice invite Bob
    println!("Alice inviting Bob");
    let welcome = a_user
        .invite(b_user.user_wallet_address().as_slice(), group_name.clone())
        .await;
    assert!(welcome.is_ok());
    // Alice should skip message with invite update because she already update her instance
    // It is failed because of wrong epoch
    let res = a_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_err());

    //// Send welcome message to system broadcast. Only Bob can use it
    m.broadcast(welcome.unwrap());
    let _ = a_r.recv();
    let welc = b_r.recv();
    assert!(welc.is_ok());
    let _ = match welc.unwrap().extract() {
        MlsMessageInBody::Welcome(welcome) => {
            let res = b_user.join_group(welcome).await;
            assert!(res.is_ok());
            Ok(())
        }
        _ => Err("do nothing".to_string()),
    };
    println!("Bob successfully join to the group");
    /////

    //// Bob send message and Alice recieve it
    let res = b_user.send_msg("Hi!", group_name.clone()).await;
    assert!(res.is_ok());

    // Bob also get the message but he cant decrypt it (regarding the mls rfc)
    let res = b_user.recieve_msg(group_name.clone()).await;
    // Expected error with invalid decryption
    assert!(res.is_err());

    let res = a_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_ok());
    /////

    //// Alice send message and Bob recieve it
    let res = a_user.send_msg("Hi Bob!", group_name.clone()).await;
    assert!(res.is_ok());

    let res = a_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_err());

    let res = b_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_ok());
    /////

    let msg = a_user.read_msgs(group_name.clone());
    println!("Alice recieve_msgs: {:#?}", msg);
    let msg = b_user.read_msgs(group_name.clone());
    println!("Bob recieve_msgs: {:#?}", msg);

    let (carla_address, carla_wallet) = carla_addr_test();
    let carla_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(carla_wallet)
        .on_http(Url::from_str("http://localhost:8545").unwrap());

    let mut c_r = m.add_rx();
    //// Create user Alice
    println!("Start Register Carla");
    let res = User::new(
        carla_address.as_slice(),
        carla_provider.clone(),
        storage_address,
    )
    .await;
    assert!(res.is_ok());
    let mut c_user = res.unwrap();
    let res = c_user.register().await;
    assert!(res.is_ok());
    println!("Register Carla successfully");
    //////

    //// Alice invite Carla
    println!("Alice inviting Carla");
    let welcome = a_user
        .invite(c_user.user_wallet_address().as_slice(), group_name.clone())
        .await;
    assert!(welcome.is_ok());
    // Alice should skip message with invite update because she already update her instance
    // It is failed because of wrong epoch
    let res = a_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_err());
    let res = b_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_ok());

    //// Send welcome message to system broadcast. Only Bob can use it
    m.broadcast(welcome.unwrap());
    let _ = a_r.recv();
    let _ = b_r.recv();
    let welc = c_r.recv();
    assert!(welc.is_ok());
    let _ = match welc.unwrap().extract() {
        MlsMessageInBody::Welcome(welcome) => {
            let res = c_user.join_group(welcome).await;
            assert!(res.is_ok());
            Ok(())
        }
        _ => Err("do nothing".to_string()),
    };
    println!("Carla successfully join to the group");
    /////

    //// Carla send message and Alice and Bob recieve it
    let res = c_user.send_msg("Hi all!", group_name.clone()).await;
    assert!(res.is_ok());

    let res = c_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_err());

    let res = a_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_ok());

    let res = b_user.recieve_msg(group_name.clone()).await;
    assert!(res.is_ok());
    ////

    let msg = a_user.read_msgs(group_name.clone());
    println!("Alice recieve_msgs: {:#?}", msg);
    let msg = b_user.read_msgs(group_name.clone());
    println!("Bob recieve_msgs: {:#?}", msg);
    let msg = c_user.read_msgs(group_name.clone());
    println!("Carla recieve_msgs: {:#?}", msg);
}
