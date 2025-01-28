use alloy::signers::local::PrivateKeySigner;
use criterion::{criterion_group, criterion_main, Criterion};
use itertools::Itertools;
use openmls::prelude::KeyPackage;
use rand::Rng;
use tokio;

use de_mls::user::User;

async fn setup_group(n: usize) -> (User, Vec<User>, String) {
    let group_name = "group".to_string() + &rand::thread_rng().gen::<u64>().to_string();
    let signer = PrivateKeySigner::random();
    let mut alice = User::new(&signer.to_bytes().to_string()).unwrap();

    alice
        .create_group(group_name.clone(), true)
        .await
        .expect("Failed to create group");

    let mut users = Vec::with_capacity(n);
    let mut key_packages = Vec::with_capacity(n);
    for _ in 0..n {
        let mut user = User::new(&signer.to_bytes().to_string()).unwrap();
        let key_package = user.new_key_package().unwrap();
        user.create_group(group_name.clone(), false)
            .await
            .expect("Failed to create group");
        users.push(user);
        key_packages.push(key_package);
    }

    // Get commit and welcome messages
    let msgs = alice
        .invite_users(key_packages.clone(), group_name.clone())
        .await
        .expect("Failed to invite users");

    let res = msgs[1].build_waku_message();
    assert!(res.is_ok(), "Failed to build waku message");
    let waku_welcome_message = res.unwrap();

    futures::future::join_all(
        users
            .iter_mut()
            .map(|user| user.process_waku_msg(waku_welcome_message.clone())),
    )
    .await
    .into_iter()
    .all(|result| result.is_ok());

    (alice, users, group_name)
}

async fn generate_users_chunk(n: usize, group_name: String) -> (Vec<User>, Vec<KeyPackage>) {
    let mut users = Vec::with_capacity(n);
    let mut key_packages = Vec::with_capacity(n);
    for _ in 0..n {
        let signer = PrivateKeySigner::random();
        let mut user = User::new(&signer.to_bytes().to_string()).unwrap();
        user.create_group(group_name.clone(), false)
            .await
            .expect("Failed to create group");
        key_packages.push(user.new_key_package().unwrap());
        users.push(user);
    }
    (users, key_packages)
}

fn multi_user_apply_commit_benchmark(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    for (i, j) in [1, 10, 100]
        .into_iter()
        .cartesian_product([1, 10, 100, 1000, 5000, 10000])
    {
        let (mut alice, mut users, group_name) = rt.block_on(async { setup_group(i).await });
        c.bench_function(
            format!("multi_user_apply_commit_benchmark_{}_{}", i, j).as_str(),
            |b| {
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total_duration = std::time::Duration::ZERO;

                        for _ in 0..iters {
                            // Setup phase: generate data
                            let (_, kp_to_join) = generate_users_chunk(j, group_name.clone()).await;

                            let start = std::time::Instant::now();
                            let msgs = alice
                                .invite_users(kp_to_join, group_name.clone())
                                .await
                                .expect("Failed to invite users");

                            let commit_msg = msgs[0].build_waku_message().unwrap();
                            futures::future::join_all(
                                users
                                    .iter_mut()
                                    .map(|user| user.process_waku_msg(commit_msg.clone())),
                            )
                            .await
                            .into_iter()
                            .all(|result| result.is_ok());
                            total_duration += start.elapsed();
                        }

                        total_duration
                    })
                })
            },
        );
    }
}

fn multi_user_welcome_benchmark(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    for (i, j) in [1, 10, 100]
        .into_iter()
        .cartesian_product([1, 10, 100, 1000, 5000, 10000])
    {
        let (mut alice, _, group_name) = rt.block_on(async { setup_group(i).await });
        c.bench_function(
            format!("multi_user_welcome_benchmark_{}_{}", i, j).as_str(),
            |b| {
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total_duration = std::time::Duration::ZERO;

                        for _ in 0..iters {
                            // Setup phase: generate data
                            let (mut users_to_join, kp_to_join) =
                                generate_users_chunk(j, group_name.clone()).await;

                            let start = std::time::Instant::now();
                            let msgs = alice
                                .invite_users(kp_to_join, group_name.clone())
                                .await
                                .expect("Failed to invite users");

                            let welcome_msg = msgs[1].build_waku_message().unwrap();
                            futures::future::join_all(
                                users_to_join
                                    .iter_mut()
                                    .map(|user| user.process_waku_msg(welcome_msg.clone())),
                            )
                            .await
                            .into_iter()
                            .all(|result| result.is_ok());
                            total_duration += start.elapsed();
                        }

                        total_duration
                    })
                })
            },
        );
    }
}

fn multi_user_generate_invite_benchmark(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    for (i, j) in [1, 10, 100, 1000]
        .into_iter()
        .cartesian_product([1, 10, 100, 1000, 5000, 10000])
    {
        let (mut alice, _, group_name) = rt.block_on(async { setup_group(i).await });
        c.bench_function(
            format!("multi_user_generate_invite_benchmark_{}_{}", i, j).as_str(),
            |b| {
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total_duration = std::time::Duration::ZERO;

                        for _ in 0..iters {
                            // Setup phase: generate data
                            let (_, kp_to_join) = generate_users_chunk(j, group_name.clone()).await;

                            let start = std::time::Instant::now();
                            let _ = alice
                                .invite_users(kp_to_join, group_name.clone())
                                .await
                                .expect("Failed to invite users");
                            total_duration += start.elapsed();
                        }

                        total_duration
                    })
                })
            },
        );
    }
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(10);
    targets =
        multi_user_apply_commit_benchmark,
        multi_user_welcome_benchmark,
        multi_user_generate_invite_benchmark
);
criterion_main!(benches);
