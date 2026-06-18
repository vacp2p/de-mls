//! ui_bridge
//!
//! Owns the command loop translating `AppCmd` -> core calls
//! and pushing `AppEvent` back to the UI via the Gateway.
//!
//! It ensures there is a Tokio runtime (desktop app may not have one yet).

use std::sync::Arc;

use de_mls::protos::de_mls::messages::v1::ConversationMessage;
use de_mls_ds::WakuDeliveryService;
use de_mls_gateway::{CoreCtx, GATEWAY, init_core};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, unbounded},
};

/// Call once during process startup (before launching the Dioxus UI).
pub fn start_ui_bridge(core: Arc<CoreCtx<WakuDeliveryService>>) {
    // 1) Give the gateway access to the core context.
    init_core(core);

    // 2) Create a command channel UI -> gateway and register the sender.
    let (cmd_tx, cmd_rx) = unbounded::<AppCmd>();
    GATEWAY.register_cmd_sink(cmd_tx);

    // 3) Drive the dispatcher loop on a Tokio runtime
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(e) = ui_loop(cmd_rx).await {
                tracing::error!("ui_loop crashed: {e}");
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
                    Err(e) => GATEWAY.push_event(AppEvent::Error(format!("Login failed: {e}"))),
                }
            }

            // ───────────── Groups ─────────────
            AppCmd::ListGroups => {
                let groups = GATEWAY.group_list().await;
                GATEWAY.push_event(AppEvent::Groups(groups));
            }

            AppCmd::CreateGroup {
                conversation_id: name,
            } => {
                if let Err(e) = GATEWAY.create_conversation(name.clone()).await {
                    GATEWAY.push_event(AppEvent::Error(format!("Create group failed: {e}")));
                    continue;
                }

                let groups = GATEWAY.group_list().await;
                GATEWAY.push_event(AppEvent::Groups(groups));

                // Push initial state (Working for steward)
                if let Ok(state) = GATEWAY.get_group_state(name.clone()).await {
                    GATEWAY.push_event(AppEvent::GroupStateChanged {
                        conversation_id: name,
                        state,
                    });
                }
            }

            AppCmd::JoinGroup { conversation_id } => {
                if let Err(e) = GATEWAY.join_group(conversation_id.clone()).await {
                    GATEWAY.push_event(AppEvent::Error(format!("Join group failed: {e}")));
                    continue;
                }

                let groups = GATEWAY.group_list().await;
                GATEWAY.push_event(AppEvent::Groups(groups));

                // Push initial state (PendingJoin for joining member)
                if let Ok(state) = GATEWAY.get_group_state(conversation_id.clone()).await {
                    GATEWAY.push_event(AppEvent::GroupStateChanged {
                        conversation_id,
                        state,
                    });
                }
            }

            AppCmd::EnterGroup { conversation_id } => {
                GATEWAY.push_event(AppEvent::EnteredGroup {
                    conversation_id: conversation_id.clone(),
                });

                // Push current state when entering group
                if let Ok(state) = GATEWAY.get_group_state(conversation_id.clone()).await {
                    GATEWAY.push_event(AppEvent::GroupStateChanged {
                        conversation_id,
                        state,
                    });
                }
            }

            AppCmd::LeaveConversation { conversation_id } => {
                if let Err(e) = GATEWAY.leave_conversation(conversation_id.clone()).await {
                    GATEWAY.push_event(AppEvent::Error(format!("Leave group failed: {e}")));
                }
            }

            AppCmd::GetGroupMembers { conversation_id } => {
                match GATEWAY.members(conversation_id.clone()).await {
                    Ok(members) => {
                        GATEWAY.push_event(AppEvent::GroupMembers {
                            conversation_id,
                            members,
                        });
                    }
                    Err(e) => {
                        GATEWAY
                            .push_event(AppEvent::Error(format!("Get group members failed: {e}")));
                    }
                }
            }

            AppCmd::SendBanRequest {
                conversation_id,
                user_to_ban,
            } => {
                if let Err(e) = GATEWAY
                    .send_ban_request(conversation_id.clone(), user_to_ban.clone())
                    .await
                {
                    GATEWAY.push_event(AppEvent::Error(format!("Send ban request failed: {e}")));
                } else {
                    GATEWAY.push_event(AppEvent::ChatMessage(ConversationMessage {
                        message: "You requested to leave or ban user from the group"
                            .to_string()
                            .into_bytes(),
                        sender: b"system".to_vec(),
                        conversation_id: conversation_id.clone(),
                    }));
                }
            }

            // ───────────── Chat ─────────────
            // The local echo is deferred until the send succeeds: echoing
            // first would leave a "sent" message in the transcript that
            // never actually reached peers (e.g. during Freezing/Selection
            // when sends are blocked). A send failure is surfaced as an
            // Error alert and must NOT tear down `ui_loop` — the UI stays
            // responsive for every other command.
            AppCmd::SendMessage {
                conversation_id,
                body,
            } => {
                match GATEWAY
                    .send_message(conversation_id.clone(), body.clone())
                    .await
                {
                    Ok(()) => {
                        GATEWAY.push_event(AppEvent::ChatMessage(ConversationMessage {
                            message: body.into_bytes(),
                            sender: b"me".to_vec(),
                            conversation_id,
                        }));
                    }
                    Err(e) => {
                        GATEWAY.push_event(AppEvent::Error(format!("Message not sent: {e}")));
                    }
                }
            }

            AppCmd::LoadHistory { conversation_id } => {
                // TODO: load from storage; stub:
                GATEWAY.push_event(AppEvent::ChatMessage(ConversationMessage {
                    message: "History loaded (stub)".as_bytes().to_vec(),
                    sender: b"system".to_vec(),
                    conversation_id: conversation_id.clone(),
                }));
            }

            // ───────────── Consensus ─────────────
            AppCmd::Vote {
                conversation_id,
                proposal_id,
                choice,
            } => {
                // "User already voted" is benign — a UI race (double-click,
                // or click arrives just after the auto-vote timer fired).
                // Silently drop so the user doesn't see a surprising error
                // popup; their vote is on record regardless.
                if let Err(e) = GATEWAY
                    .vote(conversation_id.clone(), proposal_id, choice)
                    .await
                {
                    let msg = e.to_string();
                    if msg.contains("already voted") {
                        tracing::debug!(
                            group = %conversation_id,
                            proposal_id,
                            "manual vote ignored: already voted (auto-vote won the race)"
                        );
                        continue;
                    }
                    return Err(e);
                }

                GATEWAY.push_event(AppEvent::ChatMessage(ConversationMessage {
                    message: format!(
                        "Your vote ({}) has been submitted for proposal {proposal_id}",
                        if choice { "YES" } else { "NO" }
                    )
                    .as_bytes()
                    .to_vec(),
                    sender: b"system".to_vec(),
                    conversation_id: conversation_id.clone(),
                }));
            }

            AppCmd::GetCurrentEpochProposals { conversation_id } => {
                match GATEWAY
                    .get_current_epoch_proposals(conversation_id.clone())
                    .await
                {
                    Ok(proposals) => {
                        GATEWAY.push_event(AppEvent::CurrentEpochProposals {
                            conversation_id,
                            proposals,
                        });
                    }
                    Err(e) => GATEWAY.push_event(AppEvent::Error(format!(
                        "Get current epoch proposals failed: {e}"
                    ))),
                }
            }

            AppCmd::GetStewardStatus { conversation_id } => {
                match GATEWAY.get_steward_status(conversation_id.clone()).await {
                    Ok(is_steward) => {
                        GATEWAY.push_event(AppEvent::StewardStatus {
                            conversation_id,
                            is_steward,
                        });
                    }
                    Err(e) => GATEWAY
                        .push_event(AppEvent::Error(format!("Get steward status failed: {e}"))),
                }
            }

            AppCmd::GetGroupState { conversation_id } => {
                match GATEWAY.get_group_state(conversation_id.clone()).await {
                    Ok(state) => {
                        GATEWAY.push_event(AppEvent::GroupStateChanged {
                            conversation_id,
                            state,
                        });
                    }
                    Err(e) => {
                        GATEWAY.push_event(AppEvent::Error(format!("Get group state failed: {e}")));
                    }
                }
            }

            AppCmd::GetEpochHistory { conversation_id } => {
                match GATEWAY.get_epoch_history(conversation_id.clone()).await {
                    Ok(epochs) => {
                        GATEWAY.push_event(AppEvent::EpochHistory {
                            conversation_id,
                            epochs,
                        });
                    }
                    Err(e) => {
                        GATEWAY
                            .push_event(AppEvent::Error(format!("Get epoch history failed: {e}")));
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
