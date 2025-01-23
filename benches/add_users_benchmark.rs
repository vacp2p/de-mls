use alloy::signers::local::PrivateKeySigner;
use criterion::{criterion_group, criterion_main, Criterion};
use de_mls::user::User;
use itertools::Itertools;
use openmls::prelude::KeyPackage;
use rand::Rng;
use rayon::prelude::*;
use tokio::runtime::Runtime;

async fn setup_group(n: usize) -> (User, Vec<User>, String) {
    let group_name = "group".to_string() + &rand::thread_rng().gen::<u64>().to_string();
    let mut alice =
        User::new("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80").unwrap();

    alice
        .create_group(group_name.clone(), true)
        .await
        .expect("Failed to create group");

    let mut users = Vec::with_capacity(n);
    let mut key_packages = Vec::with_capacity(n);
    for _ in 0..n {
        let signer = PrivateKeySigner::random();
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

    let results: Vec<_> = futures::future::join_all(
        users
            .iter_mut()
            .map(|user| user.process_waku_msg(waku_welcome_message.clone())),
    )
    .await;
    assert!(!results.is_empty(), "No results collected");

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

async fn generate_users_to_join(
    n: usize,
    sample_size: usize,
    group_name: String,
) -> (Vec<Vec<User>>, Vec<Vec<KeyPackage>>) {
    let results: Vec<(Vec<User>, Vec<KeyPackage>)> = (0..sample_size)
        .into_par_iter() // Use parallel iterator for chunks
        .map(|_| {
            // Call the async function in a blocking manner
            let rt = tokio::runtime::Runtime::new().unwrap();
            let (users, key_packages) = rt.block_on(generate_users_chunk(n, group_name.clone()));
            (users, key_packages) // Return only the users for this chunk
        })
        .collect(); // Collect all user chunks

    let (users_chunks, key_packages_samples): (Vec<Vec<User>>, Vec<Vec<KeyPackage>>) = results
        .into_iter()
        .map(|(users, key_packages)| (users, key_packages))
        .unzip();

    (users_chunks, key_packages_samples)
}

fn multi_user_apply_commit_benchmark(c: &mut Criterion) {
    for (i, j) in [1, 10, 100]
        .into_iter()
        .cartesian_product([1, 10, 100, 1000, 5000, 10000])
    {
        let rt = Runtime::new().unwrap();
        println!("Start to setup group with {} users", i);
        let (mut alice, mut users, group_name) = rt.block_on(async { setup_group(i).await });
        println!("Start to generate {} users to join", j);
        let (_, mut kp_to_join) =
            rt.block_on(async { generate_users_to_join(j, 100, group_name.clone()).await });
        println!("Start to benchmark");
        c.bench_function(
            format!("multi_user_commit_benchmark_{}_{}", i, j).as_str(),
            |b| {
                b.iter(|| {
                    rt.block_on(async {
                        if kp_to_join.len() < j {
                            return;
                        }
                        let msgs = alice
                            .invite_users(kp_to_join.pop().unwrap(), group_name.clone())
                            .await
                            .expect("Failed to invite users");

                        let commit_msg = msgs[0].build_waku_message().unwrap();
                        let results: Vec<_> = futures::future::join_all(
                            users
                                .iter_mut()
                                .map(|user| user.process_waku_msg(commit_msg.clone())),
                        )
                        .await;
                        assert!(!results.is_empty(), "No results collected");
                    });
                })
            },
        );
    }
}

fn multi_user_apply_commit_big_benchmark(c: &mut Criterion) {
    for (i, j) in [1000]
        .into_iter()
        .cartesian_product([1, 10, 100, 1000, 5000, 10000])
    {
        let rt = Runtime::new().unwrap();
        println!("Start to setup group with {} users", i);
        let (mut alice, mut users, group_name) = rt.block_on(async { setup_group(i).await });
        println!("Start to generate {} users to join", j);
        let (_, mut kp_to_join) =
            rt.block_on(async { generate_users_to_join(j, 100, group_name.clone()).await });
        println!("Start to benchmark");
        c.bench_function(
            format!("multi_user_commit_benchmark_{}_{}", i, j).as_str(),
            |b| {
                b.iter(|| {
                    rt.block_on(async {
                        if kp_to_join.len() < j {
                            return;
                        }
                        let msgs = alice
                            .invite_users(kp_to_join.pop().unwrap(), group_name.clone())
                            .await
                            .expect("Failed to invite users");

                        let commit_msg = msgs[0].build_waku_message().unwrap();
                        let results: Vec<_> = futures::future::join_all(
                            users
                                .iter_mut()
                                .map(|user| user.process_waku_msg(commit_msg.clone())),
                        )
                        .await;
                        assert!(!results.is_empty(), "No results collected");
                    });
                })
            },
        );
    }
}

fn multi_user_welcome_benchmark(c: &mut Criterion) {
    for (i, j) in [1, 10, 100]
        .into_iter()
        .cartesian_product([1, 10, 100, 1000, 5000, 10000])
    {
        let rt = Runtime::new().unwrap();
        println!("Start to setup group with {} users", i);
        let (mut alice, _, group_name) = rt.block_on(async { setup_group(i).await });
        println!("Start to generate {} users to join", j);
        let (mut users_to_join, mut kp_to_join) =
            rt.block_on(async { generate_users_to_join(j, 100, group_name.clone()).await });
        println!("Start to benchmark");
        c.bench_function(
            format!("multi_user_welcome_benchmark_{}_{}", i, j).as_str(),
            |b| {
                b.iter(|| {
                    rt.block_on(async {
                        if kp_to_join.len() < j {
                            return;
                        }
                        let msgs = alice
                            .invite_users(kp_to_join.pop().unwrap(), group_name.clone())
                            .await
                            .expect("Failed to invite users");

                        let mut users_to_join_sample = users_to_join.pop().unwrap();
                        let welcome_msg = msgs[1].build_waku_message().unwrap();
                        let results: Vec<_> = futures::future::join_all(
                            users_to_join_sample
                                .iter_mut()
                                .map(|user| user.process_waku_msg(welcome_msg.clone())),
                        )
                        .await;
                        assert!(!results.is_empty(), "No results collected");
                    });
                })
            },
        );
    }
}

fn multi_user_welcome_big_benchmark(c: &mut Criterion) {
    for (i, j) in [1000]
        .into_iter()
        .cartesian_product([1, 10, 100, 1000, 5000, 10000])
    {
        let rt = Runtime::new().unwrap();
        println!("Start to setup group with {} users", i);
        let (mut alice, _, group_name) = rt.block_on(async { setup_group(i).await });
        println!("Start to generate {} users to join", j);
        let (mut users_to_join, mut kp_to_join) =
            rt.block_on(async { generate_users_to_join(j, 100, group_name.clone()).await });
        println!("Start to benchmark");
        c.bench_function(
            format!("multi_user_welcome_benchmark_{}_{}", i, j).as_str(),
            |b| {
                b.iter(|| {
                    rt.block_on(async {
                        if kp_to_join.len() < j {
                            return;
                        }
                        let msgs = alice
                            .invite_users(kp_to_join.pop().unwrap(), group_name.clone())
                            .await
                            .expect("Failed to invite users");

                        let mut users_to_join_sample = users_to_join.pop().unwrap();
                        let welcome_msg = msgs[1].build_waku_message().unwrap();
                        let results: Vec<_> = futures::future::join_all(
                            users_to_join_sample
                                .iter_mut()
                                .map(|user| user.process_waku_msg(welcome_msg.clone())),
                        )
                        .await;
                        assert!(!results.is_empty(), "No results collected");
                    });
                })
            },
        );
    }
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(10);
    targets =
        multi_user_apply_commit_big_benchmark,
        multi_user_welcome_big_benchmark,
        multi_user_apply_commit_benchmark,
        multi_user_welcome_benchmark,
);
criterion_main!(benches);
