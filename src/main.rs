mod conversation;
mod identity;
mod openmls_provider;
mod user;

use std::str::FromStr;

use ds::keystore::PublicKeyStorage;
use user::User;

fn main() {
    // Define ciphersuite ...
    // let ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
    let pks = PublicKeyStorage::new();

    // MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519

    let mut a_user = User::new("Alise".as_bytes());
    let res = a_user.register(&pks);
    println!("Register a_user RES: {:?}", res);
    let mut b_user = User::new("Bob".as_bytes());
    let res = b_user.register(&pks);
    println!("Register b_user RES: {:?}", res);

    let group_name = String::from_str("Alice_Group").unwrap();
    let res = a_user.create_group(group_name.clone());
    println!("Create group RES: {:?}", res);
    println!(
        "Debug group: {:#?}",
        a_user.groups.borrow().get("Alice_Group")
    );

    let res = a_user.invite(b_user.username(), group_name.clone(), &pks);
    println!("RES: {:?}", res);

    let res = b_user.send_msg("Hi!", group_name.clone(), &pks);
    println!("RES: {:?}", res);

    let msg = a_user.recieve_msg(group_name.clone(), &pks);
    println!("RES: {:?}", msg);
}
