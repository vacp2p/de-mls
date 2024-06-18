mod conversation;
mod identity;
mod openmls_provider;
mod user;

use std::{rc::Rc, str::FromStr};

use bus::Bus;
use ds::keystore::PublicKeyStorage;
use openmls::framing::{MlsMessageIn, MlsMessageInBody};
use user::User;

fn main() {
    let pks = PublicKeyStorage::new();
    // This channel for message before adding to group.
    // Message are still encrypted, but this channel not attached to any group
    let mut m: Bus<MlsMessageIn> = Bus::new(10);
    let mut a_r = m.add_rx();
    let mut b_r = m.add_rx();

    //// Create user Alice
    println!("Start Register Alice");
    let mut a_user = User::new("Alice".as_bytes()).unwrap();
    let res = a_user.register(&pks);
    assert!(res.is_ok());
    println!("Register Alice successfully");
    //////

    //// Create user Bob
    println!("Start Register Bob");
    let mut b_user = User::new("Bob".as_bytes()).unwrap();
    let res = b_user.register(&pks);
    assert!(res.is_ok());
    println!("Register Bob successfully");
    //////

    //// Alice create group: Alice_Group
    println!("Start create group");
    let group_name = String::from_str("Alice_Group").unwrap();
    let res = a_user.create_group(group_name.clone());
    assert!(res.is_ok());
    assert!(a_user.groups.borrow().contains_key("Alice_Group"));
    println!("Create group successfully");
    //////

    //// Alice invite Bob
    println!("Alice inviting Bob");
    let welcome = a_user.invite(b_user.username(), group_name.clone(), &pks);
    assert!(welcome.is_ok());
    // Alice should skip message with invite update because she already update her instance
    // It is failed because of wrong epoch
    let res = a_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_err());

    //// Send welcome message to system broadcast. Only Bob can use it
    m.broadcast(welcome.unwrap());
    let _ = a_r.recv();
    let welc = b_r.recv();
    assert!(welc.is_ok());
    let _ = match welc.unwrap().extract() {
        MlsMessageInBody::Welcome(welcome) => {
            let res = b_user.join_group(
                welcome,
                // same ds_node, need to think how to process this
                Rc::clone(&a_user.groups.borrow().get("Alice_Group").unwrap().ds_node),
            );
            assert!(res.is_ok());
            assert!(b_user.groups.borrow().contains_key("Alice_Group"));
            Ok(())
        }
        _ => Err("do nothing".to_string()),
    };
    println!("Bob successfully join to the group");
    /////

    //// Bob send message and Alice recieve it
    let res = b_user.send_msg("Hi!", group_name.clone());
    assert!(res.is_ok());

    // Bob also get the message but he cant decrypt it (regarding the mls rfc)
    let res = b_user.recieve_msg(group_name.clone(), &pks);
    // Expected error with invalid decryption
    assert!(res.is_err());

    let res = a_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_ok());
    /////

    //// Alice send message and Bob recieve it
    let res = a_user.send_msg("Hi Bob!", group_name.clone());
    assert!(res.is_ok());

    let res = a_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_err());

    let res = b_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_ok());
    /////

    let msg = a_user.read_msgs(group_name.clone());
    println!("Alice recieve_msgs: {:?}", msg);
    let msg = b_user.read_msgs(group_name.clone());
    println!("Bob recieve_msgs: {:?}", msg);

    let mut c_r = m.add_rx();
    //// Create user Alice
    println!("Start Register Carla");
    let mut c_user = User::new("Carla".as_bytes()).unwrap();
    let res = c_user.register(&pks);
    assert!(res.is_ok());
    println!("Register Carla successfully");
    //////

    //// Alice invite Carla
    println!("Alice inviting Carla");
    let welcome = a_user.invite(c_user.username(), group_name.clone(), &pks);
    assert!(welcome.is_ok());
    // Alice should skip message with invite update because she already update her instance
    // It is failed because of wrong epoch
    let res = a_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_err());
    let res = b_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_ok());

    //// Send welcome message to system broadcast. Only Bob can use it
    m.broadcast(welcome.unwrap());
    let _ = a_r.recv();
    let _ = b_r.recv();
    let welc = c_r.recv();
    assert!(welc.is_ok());
    let _ = match welc.unwrap().extract() {
        MlsMessageInBody::Welcome(welcome) => {
            let res = c_user.join_group(
                welcome,
                // same ds_node, need to think how to process this
                Rc::clone(&a_user.groups.borrow().get("Alice_Group").unwrap().ds_node),
            );
            assert!(res.is_ok());
            assert!(c_user.groups.borrow().contains_key("Alice_Group"));
            Ok(())
        }
        _ => Err("do nothing".to_string()),
    };
    println!("Carla successfully join to the group");
    /////

    //// Carla send message and Alice and Bob recieve it
    let res = c_user.send_msg("Hi all!", group_name.clone());
    assert!(res.is_ok());

    let res = c_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_err());

    let res = a_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_ok());

    let res = b_user.recieve_msg(group_name.clone(), &pks);
    assert!(res.is_ok());
    ////

    let msg = a_user.read_msgs(group_name.clone());
    println!("Alice recieve_msgs: {:?}", msg);
    let msg = b_user.read_msgs(group_name.clone());
    println!("Bob recieve_msgs: {:?}", msg);
    let msg = c_user.read_msgs(group_name.clone());
    println!("Carla recieve_msgs: {:?}", msg);
}
