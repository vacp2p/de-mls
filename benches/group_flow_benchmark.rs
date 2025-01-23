use criterion::{criterion_group, criterion_main, Criterion};
use de_mls::user::{ProcessCreateGroup, User, UserAction};
use rand::Rng;
use tokio::runtime::Runtime;

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
                    .ask(ProcessCreateGroup {
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
                    .ask(ProcessCreateGroup {
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
                let alice_ga_msg = alice
                    .prepare_admin_msg(group_name.clone())
                    .await
                    .expect("Failed to prepare admin message");

                let waku_ga_message = alice_ga_msg
                    .build_waku_message()
                    .expect("Failed to build waku message");

                // Bob receives the Group Announcement msg and send Key Package Share msg to Alice
                let bob_res = bob
                    .process_waku_msg(waku_ga_message)
                    .await
                    .expect("Failed to process waku message");
                let bob_kp_message = match bob_res[0].clone() {
                    UserAction::SendToWaku(msg) => msg,
                    _ => panic!("User action is not SendToWaku"),
                };
                let waku_kp_message = bob_kp_message
                    .build_waku_message()
                    .expect("Failed to build waku message");

                // Alice receives the Key Package Share msg and send Welcome msg to Bob
                let user_action_invite = alice
                    .process_waku_msg(waku_kp_message)
                    .await
                    .expect("Failed to process waku message");

                let alice_welcome_message = match user_action_invite[1].clone() {
                    UserAction::SendToWaku(msg) => msg,
                    _ => panic!("User action is not SendToWaku"),
                };
                let waku_welcome_message = alice_welcome_message
                    .build_waku_message()
                    .expect("Failed to build waku message");

                // Bob receives the Welcome msg and join the group
                bob.process_waku_msg(waku_welcome_message)
                    .await
                    .expect("Failed to process waku message");
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
);
criterion_main!(benches);
