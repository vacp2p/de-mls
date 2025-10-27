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

use de_mls::protos::de_mls::messages::v1::ConversationMessage;
use de_mls::user_app_instance::CoreCtx;
use de_mls_gateway::{init_core, GATEWAY};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};

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
                GATEWAY.push_event(AppEvent::ChatMessage(ConversationMessage {
                    message: "History loaded (stub)".as_bytes().to_vec(),
                    sender: "system".to_string(),
                    group_name: group_id.clone(),
                }));
            }

            // ───────────── Chat ─────────────
            AppCmd::SendMessage { group_id, body } => {
                GATEWAY.push_event(AppEvent::ChatMessage(ConversationMessage {
                    message: body.as_bytes().to_vec(),
                    sender: "me".to_string(),
                    group_name: group_id.clone(),
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

                GATEWAY.push_event(AppEvent::ChatMessage(ConversationMessage {
                    message: format!(
                        "Your vote ({}) has been submitted for proposal {proposal_id}",
                        if choice { "YES" } else { "NO" }
                    )
                    .as_bytes()
                    .to_vec(),
                    sender: "system".to_string(),
                    group_name: group_id.clone(),
                }));
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

            AppCmd::GetCurrentEpochProposals { group_id } => {
                match GATEWAY.get_current_epoch_proposals(group_id.clone()).await {
                    Ok(proposals) => {
                        GATEWAY.push_event(AppEvent::CurrentEpochProposals {
                            group_id,
                            proposals,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("get_current_epoch_proposals failed: {e:?}");
                        GATEWAY.push_event(AppEvent::Error(
                            "Get current epoch proposals failed".into(),
                        ));
                    }
                }
            }

            AppCmd::GetGroupMembers { group_id } => {
                match GATEWAY.get_group_members(group_id.clone()).await {
                    Ok(members) => {
                        GATEWAY.push_event(AppEvent::GroupMembers { group_id, members });
                    }
                    Err(e) => {
                        tracing::warn!("get_group_members failed: {e:?}");
                        GATEWAY.push_event(AppEvent::Error("Get group members failed".into()));
                    }
                }
            }

            AppCmd::SendBanRequest {
                group_id,
                user_to_ban,
            } => {
                if let Err(e) = GATEWAY
                    .send_ban_request(group_id.clone(), user_to_ban.clone())
                    .await
                {
                    tracing::warn!("send_ban_request failed: {e:?}");
                    GATEWAY.push_event(AppEvent::Error("Send ban request failed".into()));
                } else {
                    // optional local ack
                    GATEWAY.push_event(AppEvent::ChatMessage(ConversationMessage {
                        message: "You requested to leave the group".to_string().into_bytes(),
                        sender: "system".to_string(),
                        group_name: group_id.clone(),
                    }));
                }
            }

            other => {
                tracing::warn!("unhandled AppCmd: {:?}", other);
            }
        }
    }
    Ok(())
}
