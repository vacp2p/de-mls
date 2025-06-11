use alloy::primitives::{Address, U160};
use criterion::{criterion_group, criterion_main, Criterion};
use de_mls::user::{User, UserAction};
use de_mls::user_actor::{CreateGroupRequest};
use mls_crypto::openmls_provider::{MlsCryptoProvider, CIPHERSUITE};
use openmls::prelude::{
    Credential, CredentialType, CredentialWithKey, CryptoConfig, KeyPackage, ProtocolVersion,
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::OpenMlsCryptoProvider;
use rand::{thread_rng, Rng};
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;

fn generate_random_key_package() -> KeyPackage {
    let rand_bytes = thread_rng().gen::<[u8; 20]>();
    let rand_wallet_address = Address::from(U160::from_be_bytes(rand_bytes));
    let credential = Credential::new(
        rand_wallet_address.to_string().as_bytes().to_vec(),
        CredentialType::Basic,
    )
    .unwrap();
    let signature_keys = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm()).unwrap();
    let credential_with_key = CredentialWithKey {
        credential,
        signature_key: signature_keys.to_public_vec().into(),
    };
    let crypto = &MlsCryptoProvider::default();
    signature_keys.store(crypto.key_store()).unwrap();
    KeyPackage::builder()
        .build(
            CryptoConfig {
                ciphersuite: CIPHERSUITE,
                version: ProtocolVersion::default(),
            },
            crypto,
            &signature_keys,
            credential_with_key.clone(),
        )
        .unwrap()
}

/// Benchmark for creating user with group - that means it creates mls group instance
fn create_user_with_group_benchmark(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    c.bench_function("create_user_with_group", |b| {
        b.iter(|| {
            rt.block_on(async {
                let user =
                    User::new("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                        .unwrap();
                let user_ref = kameo::spawn(user);
                user_ref
                    .ask(CreateGroupRequest {
                        group_name: "group".to_string(),
                        is_creation: true,
                    })
                    .await
                    .expect("Failed to create group");
            });
        });
    });
}

fn create_group_benchmark(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let user_ref = rt.block_on(async {
        let user = User::new("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
            .unwrap();
        kameo::spawn(user)
    });

    c.bench_function("create_group", |b| {
        b.iter(|| {
            rt.block_on(async {
                let group_name = "group".to_string() + &rand::thread_rng().gen::<u64>().to_string();
                user_ref
                    .ask(CreateGroupRequest {
                        group_name,
                        is_creation: true,
                    })
                    .await
                    .expect("Failed to create group");
            });
        });
    });
}

/// Benchmark for creating user
fn create_user_benchmark(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    c.bench_function("create_user", |b| {
        b.iter(|| {
            rt.block_on(async {
                User::new("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                    .unwrap();
            });
        });
    });
}

fn add_user_to_group_benchmark(c: &mut Criterion) {
    for i in [10, 100, 500, 1000] {
        let rt = Runtime::new().unwrap();
        let user_kps = Arc::new(Mutex::new(
            (0..i)
                .map(|_| generate_random_key_package())
                .collect::<Vec<KeyPackage>>(),
        ));
        c.bench_function(format!("add_users_to_group_{}", i).as_str(), |b| {
            b.iter(|| {
                rt.block_on(async {
                    let mut alice = User::new(
                        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
                    )
                    .unwrap();
                    alice
                        .create_group("group".to_string(), true)
                        .await
                        .expect("Failed to create group");
                });
            });
        });
    }
}

fn share_kp_benchmark(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut alice = User::new("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
        .expect("Failed to create user");
    let mut bob = User::new("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
        .expect("Failed to create user");

    c.bench_function("share_kp_benchmark", |b| {
        b.iter(|| {
            rt.block_on(async {
                let group_name = "group".to_string() + &rand::thread_rng().gen::<u64>().to_string();
                alice
                    .create_group(group_name.clone(), true)
                    .await
                    .expect("Failed to create group");
                bob.create_group(group_name.clone(), false)
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

                let _ = alice
                    .process_waku_message(bob_kp_waku_message)
                    .await
                    .expect("Failed to process waku message");

                let users_to_invite = alice
                    .group_drain_pending_proposals(group_name.clone())
                    .await
                    .expect("Failed to process income key packages");

                let res = alice
                    .process_proposals(group_name.clone(), users_to_invite)
                    .await
                    .expect("Failed to invite users");

                // let _ = bob
                //     .handle_waku_message(
                //         res.build_waku_message()
                //             .expect("Failed to build waku message"),
                //     )
                //     .await
                //     .expect("Failed to process waku message");
            });
        });
    });
}

criterion_group!(
    benches,
    create_group_benchmark,
    create_user_with_group_benchmark,
    share_kp_benchmark,
    create_user_benchmark,
    add_user_to_group_benchmark,
);
criterion_main!(benches);
