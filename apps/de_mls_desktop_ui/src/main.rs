#![allow(non_snake_case)]
use dioxus::prelude::*;
use dioxus_desktop::{Config, LogicalSize, WindowBuilder, launch::launch as desktop_launch};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use de_mls::{
    app::convert_group_request_to_display,
    mls_crypto::parse_wallet_address,
    protos::de_mls::messages::v1::{ConversationMessage, VotePayload},
};
use de_mls_gateway::{GATEWAY, bootstrap_core_from_env};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent, MemberInfo};
use hashgraph_like_consensus::types::ConsensusEvent;

mod logging;

static CSS: Asset = asset!("/assets/main.css");
static NEXT_ALERT_ID: AtomicU64 = AtomicU64::new(1);
const MAX_VISIBLE_ALERTS: usize = 5;
const MAX_VISIBLE_REJECTED: usize = 20;

// ─────────────────────────── App state ───────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
struct SessionState {
    address: String,
    key: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct GroupsState {
    items: Vec<String>, // names only
    loaded: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ChatState {
    opened_group: Option<String>,       // which group is “Open” in the UI
    messages: Vec<ConversationMessage>, // all messages; filtered per view
    members: Vec<MemberInfo>,           // cached member info for opened group
}

#[derive(Clone, Debug, PartialEq)]
struct RejectedProposal {
    action: String,
    address: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ConsensusState {
    is_steward: bool,
    group_state: String,
    pending: Option<VotePayload>,
    approved_queue: Vec<(String, String)>,
    rejected: Vec<RejectedProposal>,
    epoch_history: Vec<Vec<(String, String)>>,
    /// Caches proposal content so we can correlate with ProposalDecided events.
    proposal_cache: HashMap<u32, (String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
struct Alert {
    id: u64,
    message: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct AlertsState {
    errors: Vec<Alert>,
}

fn record_error(alerts: &mut Signal<AlertsState>, message: impl Into<String>) {
    let raw = message.into();
    let summary = summarize_error(&raw);
    tracing::error!("ui error: {}", raw);
    let id = NEXT_ALERT_ID.fetch_add(1, Ordering::Relaxed);
    let mut state = alerts.write();
    state.errors.push(Alert {
        id,
        message: summary,
    });
    if state.errors.len() > MAX_VISIBLE_ALERTS {
        state.errors.remove(0);
    }
}

fn dismiss_error(alerts: &mut Signal<AlertsState>, alert_id: u64) {
    alerts.write().errors.retain(|alert| alert.id != alert_id);
}

fn summarize_error(raw: &str) -> String {
    let mut summary = raw
        .lines()
        .next()
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| raw.trim().to_string());
    const MAX_LEN: usize = 160;
    if summary.len() > MAX_LEN {
        summary.truncate(MAX_LEN.saturating_sub(1));
        summary.push('…');
    }
    if summary.is_empty() {
        "Unexpected error".to_string()
    } else {
        summary
    }
}

/// Normalize an address for display — ensures `0x` prefix is present, no truncation.
fn fmt_addr(addr: &str) -> String {
    let hex = addr.trim_start_matches("0x").trim_start_matches("0X");
    if hex.is_empty() {
        return addr.to_string();
    }
    format!("0x{}", hex)
}

// ─────────────────────────── Routing ───────────────────────────

#[derive(Routable, Clone, PartialEq)]
enum Route {
    #[route("/")]
    Login,
    #[route("/home")]
    Home, // unified page
}

// ─────────────────────────── Entry ───────────────────────────

fn main() {
    let initial_level = logging::init_logging("info");
    tracing::info!("🚀 DE-MLS Desktop UI starting… level={}", initial_level);

    // Build a small RT to run the async bootstrap before the UI
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("rt");

    rt.block_on(async {
        let boot = bootstrap_core_from_env()
            .await
            .expect("bootstrap_core_from_env failed");
        // hand CoreCtx to the gateway via the UI bridge
        ui_bridge::start_ui_bridge(boot.core.clone());
        boot.core
    });

    let config = Config::new().with_window(
        WindowBuilder::new()
            .with_title("DE-MLS Desktop UI")
            .with_inner_size(LogicalSize::new(1280, 820))
            .with_resizable(true),
    );

    tracing::info!("Launching desktop application");
    desktop_launch(App, vec![], vec![Box::new(config)]);
}

fn App() -> Element {
    use_context_provider(|| Signal::new(AlertsState::default()));
    use_context_provider(|| Signal::new(SessionState::default()));
    use_context_provider(|| Signal::new(GroupsState::default()));
    use_context_provider(|| Signal::new(ChatState::default()));
    use_context_provider(|| Signal::new(ConsensusState::default()));

    rsx! {
        document::Stylesheet { href: CSS }
        HeaderBar {}
        AlertsCenter {}
        Router::<Route> {}
    }
}

fn HeaderBar() -> Element {
    // local signal to reflect current level in the select
    let mut level = use_signal(|| std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()));
    let session = use_context::<Signal<SessionState>>();
    let my_addr = session.read().address.clone();

    let on_change = {
        move |evt: FormEvent| {
            let new_val = evt.value();
            if let Err(e) = crate::logging::set_level(&new_val) {
                tracing::warn!("failed to set log level: {}", e);
            } else {
                level.set(new_val);
            }
        }
    };

    rsx! {
        div { class: "header",
            div { class: "brand", "DE-MLS" }
            if !my_addr.is_empty() {
                span { class: "user-hint mono ellipsis", title: "{my_addr}", "{my_addr}" }
            }
            div { class: "spacer" }
            label { class: "label", "Log level" }
            select {
                class: "level",
                value: "{level}",
                oninput: on_change,
                option { value: "error", "error" }
                option { value: "warn",  "warn"  }
                option { value: "info",  "info"  }
                option { value: "debug", "debug" }
                option { value: "trace", "trace" }
            }
        }
    }
}

// ─────────────────────────── Pages ───────────────────────────

fn Login() -> Element {
    let nav = use_navigator();
    let mut session = use_context::<Signal<SessionState>>();
    let mut key = use_signal(String::new);
    let mut alerts = use_context::<Signal<AlertsState>>();

    // Local single-consumer loop: only Login() steals LoggedIn events
    use_future({
        move || async move {
            loop {
                match GATEWAY.next_event().await {
                    Some(AppEvent::LoggedIn(name)) => {
                        session.write().address = name;
                        nav.replace(Route::Home);
                        break;
                    }
                    Some(AppEvent::Error(error)) => {
                        record_error(&mut alerts, error);
                    }
                    Some(other) => {
                        tracing::debug!("login view ignored event: {:?}", other);
                    }
                    None => break,
                }
            }
        }
    });

    let oninput_key = { move |e: FormEvent| key.set(e.value()) };

    let mut on_submit = move |_| {
        let k = key.read().trim().to_string();
        if k.is_empty() {
            return;
        }
        session.write().key = k.clone();
        spawn(async move {
            let _ = GATEWAY.send(AppCmd::Login { private_key: k }).await;
        });
    };

    rsx! {
        div { class: "page login",
            h1 { "DE-MLS — Login" }
            div { class: "form-row",
                label { "Private key" }
                input {
                    r#type: "password",
                    value: "{key}",
                    oninput: oninput_key,
                    placeholder: "0x...",
                }
            }
            button { class: "primary", onclick: move |_| { on_submit(()); }, "Enter" }
        }
    }
}

fn Home() -> Element {
    let mut groups = use_context::<Signal<GroupsState>>();
    let mut chat = use_context::<Signal<ChatState>>();
    let mut cons = use_context::<Signal<ConsensusState>>();
    let mut alerts = use_context::<Signal<AlertsState>>();

    use_future({
        move || async move {
            if !groups.read().loaded {
                let _ = GATEWAY.send(AppCmd::ListGroups).await;
            }
        }
    });

    // Local event loop for handling events from the gateway
    use_future({
        move || async move {
            loop {
                match GATEWAY.next_event().await {
                    Some(AppEvent::StewardStatus {
                        group_id,
                        is_steward,
                    }) => {
                        // only update if it is the currently opened group
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().is_steward = is_steward;
                        }
                    }
                    Some(AppEvent::GroupStateChanged { group_id, state }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().group_state = state.clone();
                            // Refresh proposals and member count on every transition to Working
                            // (clears stale approved queue after a commit is applied).
                            if state == "Working" {
                                let gid = group_id.clone();
                                spawn(async move {
                                    let _ = GATEWAY
                                        .send(AppCmd::GetCurrentEpochProposals {
                                            group_id: gid.clone(),
                                        })
                                        .await;
                                    let _ = GATEWAY
                                        .send(AppCmd::GetGroupMembers { group_id: gid })
                                        .await;
                                });
                            }
                        }
                    }
                    Some(AppEvent::CurrentEpochProposals {
                        group_id,
                        proposals,
                    }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().approved_queue = proposals;
                        }
                    }
                    Some(AppEvent::GroupMembers { group_id, members }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            chat.write().members = members;
                        }
                    }
                    Some(AppEvent::EpochHistory { group_id, epochs }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().epoch_history = epochs;
                        }
                    }
                    Some(AppEvent::ProposalAdded {
                        group_id,
                        action,
                        address,
                    }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            let exists = {
                                cons.read().approved_queue.iter().any(|(a, addr)| {
                                    a == &action && addr.eq_ignore_ascii_case(&address)
                                })
                            };
                            if !exists {
                                cons.write().approved_queue.push((action, address));
                            }
                        }
                    }
                    Some(AppEvent::CurrentEpochProposalsCleared { group_id }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().approved_queue.clear();
                            // Batch was committed — re-fetch epoch history and member count
                            let gid = group_id.clone();
                            spawn(async move {
                                let _ = GATEWAY
                                    .send(AppCmd::GetEpochHistory {
                                        group_id: gid.clone(),
                                    })
                                    .await;
                                let _ = GATEWAY
                                    .send(AppCmd::GetGroupMembers { group_id: gid })
                                    .await;
                            });
                        }
                    }
                    Some(AppEvent::Groups(names)) => {
                        groups.write().items = names;
                        groups.write().loaded = true;
                    }
                    Some(AppEvent::ChatMessage(msg)) => {
                        chat.write().messages.push(msg);
                    }
                    Some(AppEvent::VoteRequested(vp)) => {
                        let opened = chat.read().opened_group.clone();
                        if opened.as_deref() == Some(vp.group_id.as_str()) {
                            let (action, address) =
                                convert_group_request_to_display(vp.payload.clone());
                            let mut c = cons.write();
                            c.proposal_cache.insert(vp.proposal_id, (action, address));
                            c.pending = Some(vp);
                        }
                    }
                    Some(AppEvent::ProposalDecided(group_id, consensus_event)) => {
                        let is_current =
                            chat.read().opened_group.as_deref() == Some(group_id.as_str());
                        let mut c = cons.write();
                        if is_current {
                            match &consensus_event {
                                ConsensusEvent::ConsensusReached {
                                    proposal_id,
                                    result,
                                    ..
                                } => {
                                    if !result {
                                        if let Some((action, address)) =
                                            c.proposal_cache.remove(proposal_id)
                                        {
                                            c.rejected.push(RejectedProposal { action, address });
                                            if c.rejected.len() > MAX_VISIBLE_REJECTED {
                                                c.rejected.remove(0);
                                            }
                                        }
                                    } else {
                                        c.proposal_cache.remove(proposal_id);
                                    }
                                }
                                ConsensusEvent::ConsensusFailed { proposal_id, .. } => {
                                    if let Some((action, address)) =
                                        c.proposal_cache.remove(proposal_id)
                                    {
                                        c.rejected.push(RejectedProposal { action, address });
                                        if c.rejected.len() > MAX_VISIBLE_REJECTED {
                                            c.rejected.remove(0);
                                        }
                                    }
                                }
                            }
                        }
                        c.pending = None;
                    }
                    Some(AppEvent::GroupRemoved(name)) => {
                        let mut g = groups.write();
                        g.items.retain(|n| n != &name);
                        if chat.read().opened_group.as_deref() == Some(name.as_str()) {
                            chat.write().opened_group = None;
                            chat.write().members.clear();
                        }
                    }
                    Some(AppEvent::Error(error)) => {
                        record_error(&mut alerts, error);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    });

    rsx! {
        div { class: "page home",
            div { class: "layout",
                GroupListSection {}
                ChatSection {}
                ConsensusSection {}
            }
        }
    }
}

fn AlertsCenter() -> Element {
    let alerts = use_context::<Signal<AlertsState>>();
    let items = alerts.read().errors.clone();
    rsx! {
        div { class: "alerts",
            for alert in items.iter() {
                AlertItem {
                    key: "{alert.id}",
                    alert_id: alert.id,
                    message: alert.message.clone(),
                }
            }
        }
    }
}

#[derive(Props, PartialEq, Clone)]
struct AlertItemProps {
    alert_id: u64,
    message: String,
}

fn AlertItem(props: AlertItemProps) -> Element {
    let mut alerts = use_context::<Signal<AlertsState>>();
    let alert_id = props.alert_id;
    let message = props.message.clone();
    let dismiss = move |_| {
        dismiss_error(&mut alerts, alert_id);
    };

    rsx! {
        div { class: "alert error",
            span { class: "message", "{message}" }
            button { class: "ghost icon", onclick: dismiss, "✕" }
        }
    }
}

// ─────────────────────────── Sections ───────────────────────────

fn GroupListSection() -> Element {
    let groups_state = use_context::<Signal<GroupsState>>();
    let mut chat = use_context::<Signal<ChatState>>();
    let mut cons = use_context::<Signal<ConsensusState>>();
    let mut show_modal = use_signal(|| false);
    let mut new_name = use_signal(String::new);
    let mut create_mode = use_signal(|| true); // true=create, false=join

    let items_snapshot: Vec<String> = groups_state.read().items.clone();
    let loaded = groups_state.read().loaded;

    let mut open_group = {
        move |name: String| {
            chat.write().opened_group = Some(name.clone());
            chat.write().members.clear();
            cons.write().group_state.clear();
            let group_id = name.clone();
            spawn(async move {
                let _ = GATEWAY
                    .send(AppCmd::EnterGroup {
                        group_id: group_id.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::LoadHistory {
                        group_id: group_id.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::GetStewardStatus {
                        group_id: group_id.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::GetGroupState {
                        group_id: group_id.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::GetCurrentEpochProposals {
                        group_id: group_id.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::GetGroupMembers {
                        group_id: group_id.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::GetEpochHistory {
                        group_id: group_id.clone(),
                    })
                    .await;
            });
        }
    };

    let mut modal_submit = {
        move |_| {
            let name = new_name.read().trim().to_string();
            if name.is_empty() {
                return;
            }
            let action_name = name.clone();
            if *create_mode.read() {
                spawn(async move {
                    let _ = GATEWAY
                        .send(AppCmd::CreateGroup {
                            group_id: action_name.clone(),
                        })
                        .await;
                    let _ = GATEWAY.send(AppCmd::ListGroups).await;
                });
            } else {
                spawn(async move {
                    let _ = GATEWAY
                        .send(AppCmd::JoinGroup {
                            group_id: action_name.clone(),
                        })
                        .await;
                    let _ = GATEWAY.send(AppCmd::ListGroups).await;
                });
            }
            open_group(name);
            new_name.set(String::new());
            show_modal.set(false);
        }
    };

    rsx! {
        div { class: "panel groups",
            h2 { "Groups" }

            if !loaded {
                div { class: "hint", "Loading groups…" }
            } else if items_snapshot.is_empty() {
                div { class: "hint", "No groups yet." }
            } else {
                ul { class: "group-list",
                    for name in items_snapshot.into_iter() {
                        li {
                            key: "{name}",
                            class: "group-row",
                            div { class: "title", "{name}" }
                            button {
                                class: "secondary",
                                onclick: move |_| { open_group(name.clone()); },
                                "Open"
                            }
                        }
                    }
                }
            }

            div { class: "footer",
                button { class: "primary", onclick: move |_| { create_mode.set(true); show_modal.set(true); }, "Create" }
                button { class: "primary", onclick: move |_| { create_mode.set(false); show_modal.set(true); }, "Join" }
            }

            if *show_modal.read() {
                Modal {
                    title: if *create_mode.read() { "Create Group".to_string() } else { "Join Group".to_string() },
                    on_close: move || { show_modal.set(false); },
                    div { class: "form-row",
                        label { "Group name" }
                        input {
                            r#type: "text",
                            value: "{new_name}",
                            oninput: move |e| new_name.set(e.value()),
                            placeholder: "mls-devs",
                        }
                    }

                    div { class: "actions",
                        button { class: "primary", onclick: move |_| { modal_submit(()); }, "Confirm" }
                        button { class: "ghost",   onclick: move |_| { show_modal.set(false); }, "Cancel" }
                    }
                }
            }
        }
    }
}

fn ChatSection() -> Element {
    let chat = use_context::<Signal<ChatState>>();
    let session = use_context::<Signal<SessionState>>();
    let mut msg_input = use_signal(String::new);
    let mut show_ban_modal = use_signal(|| false);
    let mut ban_address = use_signal(String::new);
    let mut ban_error = use_signal(|| Option::<String>::None);

    let send_msg = {
        move |_| {
            let text = msg_input.read().trim().to_string();
            if text.is_empty() {
                return;
            }
            let Some(gid) = chat.read().opened_group.clone() else {
                return;
            };

            msg_input.set(String::new());
            spawn(async move {
                let _ = GATEWAY
                    .send(AppCmd::SendMessage {
                        group_id: gid,
                        body: text,
                    })
                    .await;
            });
        }
    };

    let open_ban_modal = {
        move |_| {
            if let Some(gid) = chat.read().opened_group.clone() {
                spawn(async move {
                    let _ = GATEWAY
                        .send(AppCmd::GetGroupMembers {
                            group_id: gid.clone(),
                        })
                        .await;
                });
            }
            ban_error.set(None);
            show_ban_modal.set(true);
        }
    };

    let submit_ban_request = {
        move |_| {
            let raw = ban_address.read().to_string();
            let target = match parse_wallet_address(&raw) {
                Ok(addr) => addr.to_string(),
                Err(err) => {
                    ban_error.set(Some(err.to_string()));
                    return;
                }
            };

            let opened = chat.read().opened_group.clone();
            let Some(group_id) = opened else {
                return;
            };

            ban_error.set(None);
            show_ban_modal.set(false);
            ban_address.set(String::new());

            let addr_to_ban = target.clone();
            spawn(async move {
                let _ = GATEWAY
                    .send(AppCmd::SendBanRequest {
                        group_id: group_id.clone(),
                        user_to_ban: addr_to_ban,
                    })
                    .await;
            });
        }
    };

    let oninput_ban_address = {
        move |e: FormEvent| {
            ban_error.set(None);
            ban_address.set(e.value())
        }
    };

    let close_ban_modal = {
        move || {
            ban_address.set(String::new());
            ban_error.set(None);
            show_ban_modal.set(false);
        }
    };

    let cancel_ban_modal = {
        move |_| {
            ban_address.set(String::new());
            ban_error.set(None);
            show_ban_modal.set(false);
        }
    };

    let msgs_for_group = {
        let opened = chat.read().opened_group.clone();
        chat.read()
            .messages
            .iter()
            .filter(|m| Some(m.group_name.as_str()) == opened.as_deref())
            .cloned()
            .collect::<Vec<_>>()
    };

    let my_name = Arc::new(session.read().address.clone());
    let my_name_for_leave = my_name.clone();

    let members_snapshot = chat.read().members.clone();
    let my_address = (*my_name).clone();
    let selectable_members: Vec<MemberInfo> = members_snapshot
        .into_iter()
        .filter(|m| !m.address.eq_ignore_ascii_case(&my_address))
        .collect();

    let pick_member_handler = {
        move |member: String| {
            move |_| {
                ban_error.set(None);
                ban_address.set(member.clone());
            }
        }
    };

    rsx! {
        div { class: "panel chat",
            div { class: "chat-header",
                h2 { "Chat" }
                if let Some(gid) = chat.read().opened_group.clone() {
                    button {
                        class: "ghost mini",
                        onclick: move |_| {
                            let group_id = gid.clone();
                            let addr = my_name_for_leave.clone();
                            // Send a self-ban (leave) request: requester filled by backend
                            spawn(async move {
                                let _ = GATEWAY
                                    .send(AppCmd::SendBanRequest { group_id: group_id.clone(), user_to_ban: (*addr).clone() })
                                    .await;
                            });
                        },
                        "Leave group"
                    }
                    button {
                        class: "ghost mini",
                        onclick: open_ban_modal,
                        "Request ban"
                    }
                }
            }
            if chat.read().opened_group.is_none() {
                div { class: "hint", "Pick a group to chat." }
            } else {
                div { class: "messages",
                    for (i, m) in msgs_for_group.iter().enumerate() {
                        if (*my_name).clone() == m.sender || m.sender.eq_ignore_ascii_case("me") {
                            div { key: "{i}", class: "msg me",
                                span { class: "from", "{m.sender}" }
                                span { class: "body", "{String::from_utf8_lossy(&m.message)}" }
                            }
                        } else if m.sender.eq_ignore_ascii_case("system") {
                            div { key: "{i}", class: "msg system",
                                span { class: "body", "{String::from_utf8_lossy(&m.message)}" }
                            }
                        } else {
                            div { key: "{i}", class: "msg",
                                span { class: "from", "{m.sender}" }
                                span { class: "body", "{String::from_utf8_lossy(&m.message)}" }
                            }
                        }
                    }
                }
                div { class: "composer",
                    input {
                        r#type: "text",
                        value: "{msg_input}",
                        oninput: move |e| msg_input.set(e.value()),
                        placeholder: "Type a message…",
                    }
                    button { class: "primary", onclick: send_msg, "Send" }
                }
            }
        }

        if *show_ban_modal.read() {
            Modal {
                title: "Request user ban".to_string(),
                on_close: close_ban_modal,
                div { class: "form-row",
                    label { "User address" }
                    input {
                        r#type: "text",
                        value: "{ban_address}",
                        oninput: oninput_ban_address,
                        placeholder: "0x...",
                    }
                    if let Some(error) = &*ban_error.read() {
                        span { class: "input-error", "{error}" }
                    }
                }
                if selectable_members.is_empty() {
                    div { class: "hint muted", "No members loaded yet." }
                } else {
                    div { class: "member-picker",
                        span { class: "helper", "Or pick a member:" }
                        div { class: "member-list",
                            for member in selectable_members.iter() {
                                div {
                                    key: "{member.address}",
                                    class: "member-item",
                                    div { class: "member-actions",
                                        span { class: "member-id mono", "{member.address}" }
                                        span { class: "member-score mono muted", " ({member.score})" }
                                        button {
                                            class: "member-choose",
                                            onclick: pick_member_handler(member.address.clone()),
                                            "Choose"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "actions",
                    button { class: "primary", onclick: submit_ban_request, "Submit" }
                    button {
                        class: "ghost",
                        onclick: cancel_ban_modal,
                        "Cancel"
                    }
                }
            }
        }
    }
}

fn ConsensusSection() -> Element {
    let chat = use_context::<Signal<ChatState>>();
    let mut cons = use_context::<Signal<ConsensusState>>();

    let vote_yes = {
        move |_| {
            let pending_proposal = cons.read().pending.clone();
            if let Some(v) = pending_proposal {
                cons.write().pending = None;
                spawn(async move {
                    let _ = GATEWAY
                        .send(AppCmd::Vote {
                            group_id: v.group_id.clone(),
                            proposal_id: v.proposal_id,
                            choice: true,
                        })
                        .await;
                });
            }
        }
    };
    let vote_no = {
        move |_| {
            let pending_proposal = cons.read().pending.clone();
            if let Some(v) = pending_proposal {
                cons.write().pending = None;
                spawn(async move {
                    let _ = GATEWAY
                        .send(AppCmd::Vote {
                            group_id: v.group_id.clone(),
                            proposal_id: v.proposal_id,
                            choice: false,
                        })
                        .await;
                });
            }
        }
    };

    let opened = chat.read().opened_group.clone();
    let pending = cons
        .read()
        .pending
        .clone()
        .filter(|p| Some(p.group_id.as_str()) == opened.as_deref());

    let pending_display = pending.as_ref().map(|v| {
        let (action, id) = convert_group_request_to_display(v.payload.clone());
        (v.proposal_id, action, id)
    });

    let approved_snapshot = cons.read().approved_queue.clone();
    let rejected_snapshot = cons.read().rejected.clone();
    let epoch_snapshot = cons.read().epoch_history.clone();
    let epoch_count = epoch_snapshot.len();
    let has_history = !rejected_snapshot.is_empty() || !epoch_snapshot.is_empty();

    rsx! {
        div { class: "panel consensus",
            h2 { "Consensus" }

            if let Some(_group) = opened {
                // Steward, Members, State status
                div { class: "status-row",
                    div { class: "status",
                        span { class: "muted", "You are " }
                        if cons.read().is_steward {
                            span { class: "good", "a steward" }
                        } else {
                            span { class: "bad", "not a steward" }
                        }
                    }
                    div { class: "status",
                        span { class: "muted", "Members: " }
                        {
                            let count = chat.read().members.len();
                            rsx! { span { class: "value", "{count}" } }
                        }
                    }
                }
                div { class: "status-row",
                    div { class: "status",
                        span { class: "muted", "State: " }
                        {
                            let state = cons.read().group_state.clone();
                            let (class, label) = match state.as_str() {
                                "Working" => ("good", "Working"),
                                "Freezing" => ("warn", "Collecting commits"),
                                "Selection" => ("warn", "Applying commit"),
                                "Reelection" => ("bad", "Reelection"),
                                "PendingJoin" => ("warn", "Pending join"),
                                "Leaving" => ("bad", "Leaving"),
                                "" => ("muted", "Unknown"),
                                other => ("muted", other),
                            };
                            rsx! { span { class: "{class}", "{label}" } }
                        }
                    }
                }

                // State context — content depends on state and role
                div { class: "consensus-section",
                    {
                        let state = cons.read().group_state.clone();
                        let is_steward = cons.read().is_steward;
                        let n = approved_snapshot.len();
                        match state.as_str() {
                            "Working" => {
                                if is_steward {
                                    rsx! {
                                        div { class: "state-context",
                                            span { class: "state-context-header",
                                                "Approved for next commit ({n}):"
                                            }
                                            if n > 0 {
                                                div { class: "proposals-window",
                                                    for (action, address) in approved_snapshot.iter() {
                                                        {
                                                            let item_class = if action.contains("Emergency") {
                                                                "proposal-item emergency"
                                                            } else {
                                                                "proposal-item"
                                                            };
                                                            let short = fmt_addr(address);
                                                            rsx! {
                                                                div { class: "{item_class}",
                                                                    span { class: "action", "{action}:" }
                                                                    span { class: "value", "{short}" }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            } else {
                                                div { class: "no-data", "No proposals pending." }
                                            }
                                        }
                                    }
                                } else {
                                    let suffix = if n == 1 { "" } else { "s" };
                                    rsx! {
                                        div { class: "state-context",
                                            if n > 0 {
                                                span { class: "state-context-header",
                                                    "Waiting for steward to commit {n} proposal{suffix}:"
                                                }
                                                div { class: "proposals-window",
                                                    for (action, address) in approved_snapshot.iter() {
                                                        {
                                                            let item_class = if action.contains("Emergency") {
                                                                "proposal-item emergency"
                                                            } else {
                                                                "proposal-item"
                                                            };
                                                            let short = fmt_addr(address);
                                                            rsx! {
                                                                div { class: "{item_class}",
                                                                    span { class: "action", "{action}:" }
                                                                    span { class: "value", "{short}" }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            } else {
                                                span { class: "muted", "No pending proposals." }
                                            }
                                        }
                                    }
                                }
                            }
                            "Freezing" => rsx! {
                                div { class: "state-context warn",
                                    span { "⏳ Collecting commit candidates..." }
                                }
                            },
                            "Selection" => rsx! {
                                div { class: "state-context warn",
                                    span { "⚙ Applying selected commit to the group." }
                                }
                            },
                            "Reelection" => rsx! {
                                div { class: "state-context bad",
                                    span { "⚠ Steward fault detected. Regular proposals are blocked until the emergency vote resolves." }
                                }
                            },
                            "PendingJoin" => rsx! {
                                div { class: "state-context warn",
                                    span { "⌛ Waiting for welcome message..." }
                                }
                            },
                            "Leaving" => rsx! {
                                div { class: "state-context warn",
                                    span { "⌛ Waiting for removal commit..." }
                                }
                            },
                            _ => rsx! {},
                        }
                    }
                }

                // Active Vote
                div { class: "consensus-section",
                    h3 { "Active Vote" }
                    if let Some((proposal_id, action, id)) = pending_display {
                        {
                            let is_emergency = action.starts_with("Emergency: ");
                            let violation_type = if is_emergency {
                                action.strip_prefix("Emergency: ").unwrap_or("").to_string()
                            } else {
                                String::new()
                            };
                            let short_id = fmt_addr(&id);
                            if is_emergency {
                                rsx! {
                                    div { class: "proposals-window emergency",
                                        div { class: "emergency-header",
                                            span { class: "bad", "⚠ Emergency Proposal #{proposal_id}" }
                                        }
                                        div { class: "proposal-item",
                                            span { class: "action", "Violation:" }
                                            span { class: "value bad", "{violation_type}" }
                                        }
                                        div { class: "proposal-item",
                                            span { class: "action", "Accused steward:" }
                                            span { class: "value", "{short_id}" }
                                        }
                                    }
                                }
                            } else {
                                rsx! {
                                    div { class: "proposals-window",
                                        div { class: "proposal-item proposal-id",
                                            span { class: "action", "Proposal #{proposal_id}" }
                                        }
                                        div { class: "proposal-item",
                                            span { class: "action", "{action}:" }
                                            span { class: "value", "{short_id}" }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "vote-actions",
                            button { class: "primary", onclick: vote_yes, "YES" }
                            button { class: "ghost",   onclick: vote_no,  "NO"  }
                        }
                    } else {
                        div { class: "no-data", "No active vote" }
                    }
                }

                // Epoch History
                div { class: "consensus-section",
                    h3 { "Epoch History" }
                    if has_history {
                        div { class: "history-window",
                            // Rejected proposals
                            if !rejected_snapshot.is_empty() {
                                div { class: "history-group",
                                    span { class: "history-label rejected-label", "Rejected" }
                                    for rp in rejected_snapshot.iter().rev() {
                                        {
                                            let short = fmt_addr(&rp.address);
                                            rsx! {
                                                div { class: "history-entry rejected",
                                                    span { class: "action", "{rp.action}:" }
                                                    span { class: "value", "{short}" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Past epochs
                            if !epoch_snapshot.is_empty() {
                                div { class: "history-group",
                                    span { class: "history-label", "Past Epochs" }
                                    for (i, batch) in epoch_snapshot.iter().rev().enumerate() {
                                        div { class: "epoch-group",
                                            span { class: "epoch-label",
                                                "Epoch {epoch_count - i}"
                                            }
                                            for (action, address) in batch.iter() {
                                                {
                                                    let is_emerg = action.contains("Emergency");
                                                    let display_action = if is_emerg {
                                                        format!("⚠ {}", action)
                                                    } else {
                                                        action.clone()
                                                    };
                                                    let entry_class = if is_emerg {
                                                        "history-entry emergency"
                                                    } else {
                                                        "history-entry"
                                                    };
                                                    let short = fmt_addr(address);
                                                    rsx! {
                                                        div { class: "{entry_class}",
                                                            span { class: "action", "{display_action}:" }
                                                            span { class: "value", "{short}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        div { class: "no-data", "No history yet" }
                    }
                }
            } else {
                div { class: "hint", "Open a group to see proposals & voting." }
            }
        }
    }
}

// ─────────────────────────── Modal ───────────────────────────

#[derive(Props, PartialEq, Clone)]
struct ModalProps {
    title: String,
    children: Element,
    on_close: EventHandler,
}
fn Modal(props: ModalProps) -> Element {
    rsx! {
        div { class: "modal-backdrop", onclick: move |_| (props.on_close)(()),
            div { class: "modal", onclick: move |e| e.stop_propagation(),
                div { class: "modal-head",
                    h3 { "{props.title}" }
                    button { class: "icon", onclick: move |_| (props.on_close)(()), "✕" }
                }
                div { class: "modal-body", {props.children} }
            }
        }
    }
}
