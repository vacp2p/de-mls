//! ui_bridge
//!
//! Owns the command loop translating `AppCmd` -> core calls
//! and pushing `AppEvent` back to the UI via the Gateway.
//!
//! It ensures there is a Tokio runtime (desktop app may not have one yet).

// crates/ui_bridge/src/lib.rs
use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use futures::StreamExt;
use std::sync::Arc;

use de_mls::user_app_instance::CoreCtx;
use de_mls_gateway::{init_core, GATEWAY};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent, ChatMsg};

/// Call once during process startup (before launching the Dioxus UI).
pub fn start_ui_bridge(core: Arc<CoreCtx>) {
    // 1) Give the gateway access to the core context.
    init_core(core);

    // 1.5) Ensure gateway background (pubsub forwarder) is started
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async {
            if let Err(e) = de_mls_gateway::GATEWAY.start().await {
                tracing::error!("gateway start failed: {e:?}");
            }
        });
    } else {
        std::thread::Builder::new()
            .name("ui-bridge-start".into())
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(async {
                    if let Err(e) = de_mls_gateway::GATEWAY.start().await {
                        eprintln!("gateway start failed: {e:?}");
                    }
                });
            })
            .expect("spawn ui-bridge-start");
    }

    // 2) Create a command channel UI -> gateway and register the sender.
    let (cmd_tx, cmd_rx) = unbounded::<AppCmd>();
    GATEWAY.register_cmd_sink(cmd_tx);

    // 3) Drive the dispatcher loop on a Tokio runtime (unchanged)
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(e) = ui_loop(cmd_rx).await {
                tracing::error!("ui_loop crashed: {e:?}");
            }
        });
    } else {
        std::thread::Builder::new()
            .name("ui-bridge".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(async move {
                    if let Err(e) = ui_loop(cmd_rx).await {
                        eprintln!("ui_loop crashed: {e:?}");
                    }
                });
            })
            .expect("spawn ui-bridge");
    }
}

async fn ui_loop(mut cmd_rx: UnboundedReceiver<AppCmd>) -> anyhow::Result<()> {
    while let Some(cmd) = cmd_rx.next().await {
        match cmd {
            // ───────────── Authentication / session ─────────────
            AppCmd::Login { private_key } => {
                match GATEWAY.login_with_private_key(private_key).await {
                    Ok(derived_name) => GATEWAY.push_event(AppEvent::LoggedIn(derived_name)),
                    Err(e) => {
                        // Consider adding AppEvent::Error to protocol
                        tracing::error!("login failed: {e:?}");
                    }
                }
            }

            // ───────────── Groups ─────────────
            AppCmd::ListGroups => {
                let groups = GATEWAY.group_list().await?;
                GATEWAY.push_event(AppEvent::Groups(groups));
            }

            AppCmd::CreateGroup { name } => {
                GATEWAY.create_group(name.clone()).await?;

                let groups = GATEWAY.group_list().await?;
                GATEWAY.push_event(AppEvent::Groups(groups));
            }

            AppCmd::JoinGroup { name } => {
                GATEWAY.join_group(name.clone()).await?;
                let groups = GATEWAY.group_list().await?;
                GATEWAY.push_event(AppEvent::Groups(groups));
            }

            AppCmd::EnterGroup { group_id } => {
                GATEWAY.push_event(AppEvent::EnteredGroup { group_id });
            }

            AppCmd::LoadHistory { group_id } => {
                // TODO: load from storage; stub:
                GATEWAY.push_event(AppEvent::ChatMessage(ChatMsg {
                    id: uuid::Uuid::new_v4().to_string(),
                    group_id,
                    author: "system".into(),
                    body: "History loaded (stub)".into(),
                    ts_ms: now_ms(),
                }));
            }

            // ───────────── Chat ─────────────
            AppCmd::SendMessage { group_id, body } => {
                GATEWAY.push_event(AppEvent::ChatMessage(ChatMsg {
                    id: uuid::Uuid::new_v4().to_string(),
                    group_id: group_id.clone(),
                    author: "me".into(),
                    body: body.clone(),
                    ts_ms: now_ms(),
                }));

                GATEWAY.send_message(group_id, body).await?;
            }

            // ───────────── Votes ─────────────
            AppCmd::Vote {
                group_id,
                proposal_id,
                choice,
            } => {
                // Process the user vote:
                // if it come from the user, send the vote result to Waku
                // if it come from the steward, just process it and return None
                GATEWAY
                    .process_user_vote(group_id.clone(), proposal_id, choice)
                    .await?;

                GATEWAY.push_event(AppEvent::ChatMessage(ChatMsg {
                    id: uuid::Uuid::new_v4().to_string(),
                    group_id,
                    author: "system".into(),
                    body: format!(
                        "Your vote ({}) has been submitted for proposal {proposal_id}",
                        if choice { "YES" } else { "NO" }
                    ),
                    ts_ms: now_ms(),
                }));

                GATEWAY.push_event(AppEvent::VoteClosed {
                    proposal_id: proposal_id,
                });
            }
            AppCmd::LeaveGroup { group_id } => {
                GATEWAY.push_event(AppEvent::LeaveGroup { group_id });
            }

            AppCmd::QuerySteward { group_id } => {
                match GATEWAY.query_steward(group_id.clone()).await {
                    Ok(is_steward) => {
                        GATEWAY.push_event(AppEvent::StewardStatus {
                            group_id,
                            is_steward,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("query_steward failed: {e:?}");
                        GATEWAY.push_event(AppEvent::Error("Query steward failed".into()));
                    }
                }
            }

            other => {
                tracing::warn!("unhandled AppCmd: {:?}", other);
            }
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
