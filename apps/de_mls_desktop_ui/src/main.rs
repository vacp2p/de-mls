// apps/de_mls_desktop_ui/src/main.rs
#![allow(non_snake_case)]
use dioxus::prelude::*;
use dioxus_desktop::{launch::launch as desktop_launch, Config, LogicalSize, WindowBuilder};
use hashgraph_like_consensus::types::ConsensusEvent;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use de_mls::{
    app::bootstrap_core_from_env,
    core::convert_group_requests_to_display,
    protos::de_mls::messages::v1::{ConversationMessage, Outcome, VotePayload},
};
use de_mls_gateway::GATEWAY;
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};
use mls_crypto::normalize_wallet_address_str;

mod logging;

static CSS: Asset = asset!("/assets/main.css");
static NEXT_ALERT_ID: AtomicU64 = AtomicU64::new(1);
const MAX_VISIBLE_ALERTS: usize = 5;
const MAX_VISIBLE_CONSENSUS_RESULTS: usize = 50;

// Helper function to format unix timestamps (seconds since epoch)
fn format_timestamp(timestamp_s: u64) -> String {
    use std::time::UNIX_EPOCH;

    // Convert to SystemTime and format
    let timestamp = UNIX_EPOCH + std::time::Duration::from_secs(timestamp_s);
    let datetime: chrono::DateTime<chrono::Utc> = timestamp.into();
    datetime.format("%H:%M:%S").to_string()
}

fn render_latest_decision(ev: &ConsensusEvent) -> Element {
    let (vid, res, timestamp_s, label): (u32, Outcome, u64, &'static str) = match ev {
        ConsensusEvent::ConsensusReached {
            proposal_id,
            result,
            timestamp,
        } => (
            *proposal_id,
            if *result {
                Outcome::Accepted
            } else {
                Outcome::Rejected
            },
            *timestamp,
            if *result { "Accepted" } else { "Rejected" },
        ),
        ConsensusEvent::ConsensusFailed {
            proposal_id,
            timestamp,
        } => (*proposal_id, Outcome::Unspecified, *timestamp, "Failed"),
    };

    rsx! {
        div { class: "result-item",
            span { class: "proposal-id", "{vid}" }
            span {
                class: match res {
                    Outcome::Accepted => "outcome accepted",
                    Outcome::Rejected => "outcome rejected",
                    Outcome::Unspecified => "outcome unspecified",
                },
                "{label}"
            }
            span { class: "timestamp",
                "{format_timestamp(timestamp_s)}"
            }
        }
    }
}

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
    members: Vec<String>,               // cached member addresses for opened group
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ConsensusState {
    is_steward: bool,
    pending: Option<VotePayload>, // active/pending proposal for opened group
    // Store results with timestamps for better display
    latest_results: Vec<ConsensusEvent>,
    // Store current epoch proposals for stewards
    current_epoch_proposals: Vec<(String, String)>, // (action, address) pairs
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
                    Some(AppEvent::CurrentEpochProposals {
                        group_id,
                        proposals,
                    }) => {
                        // only update if it is the currently opened group
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().current_epoch_proposals = proposals;
                        }
                    }
                    Some(AppEvent::GroupMembers { group_id, members }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            chat.write().members = members;
                        }
                    }
                    Some(AppEvent::ProposalAdded {
                        group_id,
                        action,
                        address,
                    }) => {
                        // only update if it is the currently opened group
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            // Avoid duplicates: do not enqueue if the same (action, address) already exists
                            let exists = {
                                cons.read().current_epoch_proposals.iter().any(|(a, addr)| {
                                    a == &action && addr.eq_ignore_ascii_case(&address)
                                })
                            };
                            if !exists {
                                cons.write().current_epoch_proposals.push((action, address));
                            }
                        }
                    }
                    Some(AppEvent::CurrentEpochProposalsCleared { group_id }) => {
                        // only update if it is the currently opened group
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().current_epoch_proposals.clear();
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
                            cons.write().pending = Some(vp);
                        }
                    }
                    Some(AppEvent::ProposalDecided(group_id, consensus_event)) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            {
                                let mut c = cons.write();
                                c.latest_results.push(consensus_event);
                                if c.latest_results.len() > MAX_VISIBLE_CONSENSUS_RESULTS {
                                    // Drop oldest entries first
                                    let overflow =
                                        c.latest_results.len() - MAX_VISIBLE_CONSENSUS_RESULTS;
                                    c.latest_results.drain(0..overflow);
                                }
                            }
                        }
                        cons.write().pending = None;
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
    let mut show_modal = use_signal(|| false);
    let mut new_name = use_signal(String::new);
    let mut create_mode = use_signal(|| true); // true=create, false=join

    let items_snapshot: Vec<String> = groups_state.read().items.clone();
    let loaded = groups_state.read().loaded;

    let mut open_group = {
        move |name: String| {
            chat.write().opened_group = Some(name.clone());
            chat.write().members.clear();
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
                    .send(AppCmd::GetCurrentEpochProposals {
                        group_id: group_id.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::GetGroupMembers {
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
                            name: action_name.clone(),
                        })
                        .await;
                    let _ = GATEWAY.send(AppCmd::ListGroups).await;
                });
            } else {
                spawn(async move {
                    let _ = GATEWAY
                        .send(AppCmd::JoinGroup {
                            name: action_name.clone(),
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
            let target = match normalize_wallet_address_str(&raw) {
                Ok(addr) => addr,
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
    let selectable_members: Vec<String> = members_snapshot
        .into_iter()
        .filter(|member| !member.eq_ignore_ascii_case(&my_address))
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
                                    key: "{member}",
                                    class: "member-item",
                                    div { class: "member-actions",
                                        span { class: "member-id mono", "{member}" }
                                        button {
                                            class: "member-choose",
                                            onclick: pick_member_handler(member.clone()),
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
                // Clear the pending proposal immediately to close the vote window
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
                // Clear the pending proposal immediately to close the vote window
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

    rsx! {
        div { class: "panel consensus",
            h2 { "Consensus" }

            if let Some(_group) = opened {
                // Steward status
                div { class: "status",
                    span { class: "muted", "You are " }
                    if cons.read().is_steward {
                        span { class: "good", "a steward" }
                    } else {
                        span { class: "bad", "not a steward" }
                    }
                }

                // Pending Requests section
                div { class: "consensus-section",
                    h3 { "Pending Requests" }
                    if cons.read().is_steward && !cons.read().current_epoch_proposals.is_empty() {
                        div { class: "proposals-window",
                            for (action, address) in &cons.read().current_epoch_proposals {
                                div { class: "proposal-item",
                                    span { class: "action", "{action}:" }
                                    span { class: "value", "{address}" }
                                }
                            }
                        }
                    } else {
                        div { class: "no-data", "No pending requests" }
                    }
                }

                // Proposal for Vote section
                div { class: "consensus-section",
                    h3 { "Proposal for Vote" }
                    if let Some(v) = pending {
                        div { class: "proposals-window",
                            div { class: "proposal-item proposal-id",
                                span { class: "action", "Proposal ID:" }
                                span { class: "value", "{v.proposal_id}" }
                            }
                            for (action, id) in convert_group_requests_to_display(&v.group_requests) {
                                div { class: "proposal-item",
                                    span { class: "action", "{action}:" }
                                    span { class: "value", "{id}" }
                                }
                            }
                        }
                        div { class: "vote-actions",
                            button { class: "primary", onclick: vote_yes, "YES" }
                            button { class: "ghost",   onclick: vote_no,  "NO"  }
                        }
                    } else {
                        div { class: "no-data", "No proposal for vote" }
                    }
                }

                // Latest Decisions section
                div { class: "consensus-section",
                    h3 { "Latest Decisions" }
                    if cons.read().latest_results.is_empty() {
                        div { class: "no-data", "No latest decisions" }
                    } else {
                        div { class: "results-window",
                            for ev in cons.read().latest_results.iter().rev() {
                                {render_latest_decision(ev)}
                            }
                        }
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
