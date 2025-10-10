//! ui_bridge
//!
//! Owns the command loop translating `AppCmd` -> core calls
//! and pushing `AppEvent` back to the UI via the Gateway.
//!
//! It ensures there is a Tokio runtime (desktop app may not have one yet).

use std::sync::Arc;

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use futures::StreamExt;

use de_mls::CoreCtx;
use de_mls_gateway::{init_core, GATEWAY};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent, ChatMsg};

/// Call once during process startup (before launching the Dioxus UI).
pub fn start_ui_bridge(core: Arc<CoreCtx>) {
    // 1) Give the gateway access to the core context.
    init_core(core);

    // 2) Create a command channel UI -> gateway and register the sender.
    let (cmd_tx, cmd_rx) = unbounded::<AppCmd>();
    GATEWAY.register_cmd_sink(cmd_tx);

    // 3) Drive the dispatcher loop on a Tokio runtime.
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

            // ───────────── Groups (stubs or wire to actors) ─────────────
            AppCmd::ListGroups => {
                let groups = GATEWAY.group_list().await?;
                GATEWAY.push_event(AppEvent::Groups(groups));
            }

            AppCmd::CreateGroup { name } => {
                let _ = GATEWAY.create_group(name.clone()).await;
                let groups = GATEWAY.group_list().await?;
                GATEWAY.push_event(AppEvent::Groups(groups));
            }

            AppCmd::JoinGroup { name: _name } => {
                // TODO: resolve & join (subscribe to topics, etc.)
            }

            AppCmd::EnterGroup { group_id } => {
                // TODO: ensure subscription via waku and route messages back.
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
                // TODO: route to user actor -> waku; echo for now
                GATEWAY.push_event(AppEvent::ChatMessage(ChatMsg {
                    id: uuid::Uuid::new_v4().to_string(),
                    group_id,
                    author: "me".into(),
                    body,
                    ts_ms: now_ms(),
                }));
            }

            // ───────────── Votes ─────────────
            AppCmd::Vote { vote_id, choice: _ } => {
                // TODO: forward vote; when closed by backend emit VoteClosed
                GATEWAY.push_event(AppEvent::VoteClosed { vote_id });
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
