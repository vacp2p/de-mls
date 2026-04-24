#![allow(non_snake_case)]
use dioxus::prelude::*;
use dioxus_desktop::{Config, LogicalSize, WindowBuilder, launch::launch as desktop_launch};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use de_mls::{
    app::format_group_request,
    mls_crypto::parse_wallet_address,
    protos::de_mls::messages::v1::{ConversationMessage, GroupUpdateRequest, VotePayload},
};
use de_mls_gateway::{GATEWAY, bootstrap_core_from_env};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent, MemberInfo};
use hashgraph_like_consensus::types::ConsensusEvent;
use prost::Message;

mod logging;

static CSS: Asset = asset!("/assets/main.css");
static NEXT_ALERT_ID: AtomicU64 = AtomicU64::new(1);
const MAX_VISIBLE_ALERTS: usize = 5;
const MAX_VISIBLE_REJECTED: usize = 20;
const MAX_VISIBLE_ELECTIONS: usize = 20;

// ─────────────────────────── App state ───────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
struct SessionState {
    address: String,
    key: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct GroupsState {
    items: Vec<String>,
    loaded: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ChatState {
    opened_group: Option<String>,
    messages: Vec<ConversationMessage>,
    members: Vec<MemberInfo>,
}

#[derive(Clone, Debug, PartialEq)]
struct RejectedProposal {
    action: String,
    address: String,
    reason: &'static str,
}

/// Compact record of a completed steward-election attempt.
#[derive(Clone, Debug, PartialEq)]
struct ElectionRecord {
    details: String,
    accepted: bool,
    reason: Option<&'static str>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ConsensusState {
    is_steward: bool,
    group_state: String,
    epoch: u64,
    retry_round: u32,
    pending_votes: Vec<VotePayload>,
    approved_queue: Vec<(String, String)>,
    rejected: Vec<RejectedProposal>,
    elections: Vec<ElectionRecord>,
    epoch_history: Vec<Vec<(String, String)>>,
    proposal_cache: HashMap<u32, (String, String)>,
    freeze_candidates: (usize, usize),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RailTab {
    Members,
    Proposals,
    History,
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

fn fmt_addr(addr: &str) -> String {
    let hex = addr.trim_start_matches("0x").trim_start_matches("0X");
    if hex.is_empty() {
        return addr.to_string();
    }
    format!("0x{}", hex)
}

fn role_for(role: &str) -> (&'static str, &'static str) {
    match role {
        "epoch_steward" => ("role-badge epoch", "Epoch Steward"),
        "backup_steward" => ("role-badge backup", "Backup Steward"),
        "steward" => ("role-badge steward", "Steward"),
        _ => ("role-badge member", "Member"),
    }
}

/// Split the steward-election composite string `"epoch N | addr, addr"`
/// (with `, retry R` appended when `R > 0`) into a human-readable meta line
/// and a list of steward addresses. Used by the active-vote banner to render
/// stewards vertically instead of wrapping one long line of commas.
fn parse_election_details(raw: &str) -> (String, Vec<String>) {
    let (meta_raw, list_raw) = raw.split_once(" | ").unwrap_or((raw, ""));
    let meta = meta_raw
        .replace("epoch ", "Epoch ")
        .replace("retry ", "Retry ")
        .replace(", ", " · ");
    let stewards = if list_raw.is_empty() {
        Vec::new()
    } else {
        list_raw.split(", ").map(|s| s.to_string()).collect()
    };
    (meta, stewards)
}

fn state_label(state: &str) -> (&'static str, String) {
    match state {
        "Working" => ("good", "Working".to_string()),
        "Freezing" => ("warn", "Collecting commits".to_string()),
        "Selection" => ("warn", "Applying commit".to_string()),
        "Reelection" => ("bad", "Reelection".to_string()),
        "PendingJoin" => ("warn", "Pending join".to_string()),
        "Leaving" => ("bad", "Leaving".to_string()),
        "" => ("muted", "Unknown".to_string()),
        other => ("muted", other.to_string()),
    }
}

// ─────────────────────────── Routing ───────────────────────────

#[derive(Routable, Clone, PartialEq)]
enum Route {
    #[route("/")]
    Login,
    #[route("/home")]
    Home,
}

// ─────────────────────────── Entry ───────────────────────────

fn main() {
    let initial_level = logging::init_logging("info");
    tracing::info!("🚀 DE-MLS Desktop UI starting… level={}", initial_level);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("rt");

    rt.block_on(async {
        let boot = bootstrap_core_from_env()
            .await
            .expect("bootstrap_core_from_env failed");
        ui_bridge::start_ui_bridge(boot.core.clone());
        boot.core
    });

    let config = Config::new().with_window(
        WindowBuilder::new()
            .with_title("DE-MLS Desktop UI")
            .with_inner_size(LogicalSize::new(1280, 820))
            .with_min_inner_size(LogicalSize::new(1100, 680))
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
    use_context_provider(|| Signal::new(RailTab::Members));

    rsx! {
        document::Stylesheet { href: CSS }
        HeaderBar {}
        AlertsCenter {}
        Router::<Route> {}
    }
}

fn HeaderBar() -> Element {
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
                span { class: "user-chip mono ellipsis", title: "{my_addr}", "{my_addr}" }
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

    use_future({
        move || async move {
            loop {
                match GATEWAY.next_event().await {
                    Some(AppEvent::StewardStatus {
                        group_id,
                        is_steward,
                    }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().is_steward = is_steward;
                        }
                    }
                    Some(AppEvent::GroupStateChanged { group_id, state }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            {
                                let mut c = cons.write();
                                c.group_state = state.clone();
                                if state != "Freezing" {
                                    c.freeze_candidates = (0, 0);
                                }
                            }
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
                    Some(AppEvent::FreezeCandidates {
                        group_id,
                        received,
                        expected,
                    }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().freeze_candidates = (received, expected);
                        }
                    }
                    Some(AppEvent::EpochHistory { group_id, epochs }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().epoch_history = epochs;
                        }
                    }
                    Some(AppEvent::GroupEpoch {
                        group_id,
                        epoch,
                        retry_round,
                    }) => {
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            let mut c = cons.write();
                            c.epoch = epoch;
                            c.retry_round = retry_round;
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
                                GroupUpdateRequest::decode(vp.payload.as_slice())
                                    .map(|req| format_group_request(&req))
                                    .unwrap_or_else(|_| {
                                        ("Invalid".to_string(), "malformed payload".to_string())
                                    });
                            let mut c = cons.write();
                            c.proposal_cache.insert(vp.proposal_id, (action, address));
                            if !c
                                .pending_votes
                                .iter()
                                .any(|p| p.proposal_id == vp.proposal_id)
                            {
                                c.pending_votes.push(vp);
                            }
                        }
                    }
                    Some(AppEvent::OwnProposalSubmitted {
                        group_id,
                        proposal_id,
                        action,
                        address,
                    }) => {
                        let is_current =
                            chat.read().opened_group.as_deref() == Some(group_id.as_str());
                        if is_current {
                            cons.write()
                                .proposal_cache
                                .insert(proposal_id, (action, address));
                        }
                    }
                    Some(AppEvent::ProposalDecided(group_id, consensus_event)) => {
                        let is_current =
                            chat.read().opened_group.as_deref() == Some(group_id.as_str());
                        let mut c = cons.write();
                        if is_current {
                            let (accepted, reason) = match &consensus_event {
                                ConsensusEvent::ConsensusReached { result, .. } => {
                                    (*result, if *result { None } else { Some("Rejected") })
                                }
                                ConsensusEvent::ConsensusFailed { .. } => {
                                    (false, Some("Timed out"))
                                }
                            };
                            let proposal_id = match &consensus_event {
                                ConsensusEvent::ConsensusReached { proposal_id, .. } => {
                                    *proposal_id
                                }
                                ConsensusEvent::ConsensusFailed { proposal_id, .. } => *proposal_id,
                            };
                            if let Some((action, address)) = c.proposal_cache.remove(&proposal_id) {
                                if action.starts_with("Steward Election") {
                                    c.elections.push(ElectionRecord {
                                        details: address,
                                        accepted,
                                        reason,
                                    });
                                    if c.elections.len() > MAX_VISIBLE_ELECTIONS {
                                        c.elections.remove(0);
                                    }
                                } else if !accepted {
                                    c.rejected.push(RejectedProposal {
                                        action,
                                        address,
                                        reason: reason.unwrap_or("Rejected"),
                                    });
                                    if c.rejected.len() > MAX_VISIBLE_REJECTED {
                                        c.rejected.remove(0);
                                    }
                                }
                            }
                        }
                        let decided_id = match &consensus_event {
                            ConsensusEvent::ConsensusReached { proposal_id, .. } => *proposal_id,
                            ConsensusEvent::ConsensusFailed { proposal_id, .. } => *proposal_id,
                        };
                        c.pending_votes.retain(|v| v.proposal_id != decided_id);
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
            StatusStrip {}
            div { class: "layout",
                GroupListSection {}
                ChatSection {}
                RightRail {}
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

// ─────────────────────────── Status strip ───────────────────────────

fn StatusStrip() -> Element {
    let chat = use_context::<Signal<ChatState>>();
    let cons = use_context::<Signal<ConsensusState>>();
    let session = use_context::<Signal<SessionState>>();

    let opened = chat.read().opened_group.clone();
    let Some(group_name) = opened else {
        return rsx! {};
    };

    let members = chat.read().members.clone();
    let my_addr = session.read().address.clone();
    let state = cons.read().group_state.clone();
    let epoch = cons.read().epoch;
    let retry_round = cons.read().retry_round;
    let (state_cls, state_text) = state_label(&state);
    let (role_cls, role_text) = members
        .iter()
        .find(|m| m.address.eq_ignore_ascii_case(&my_addr))
        .map(|m| role_for(m.role.as_str()))
        .unwrap_or(("role-badge member", "Member"));
    let member_count = members.len();
    let epoch_label = if retry_round == 0 {
        format!("{epoch}")
    } else {
        format!("{epoch} · retry {retry_round}")
    };

    rsx! {
        div { class: "status-strip",
            span { class: "status-group", "{group_name}" }
            span { class: "status-sep" }
            span { class: "status-label", "State" }
            span { class: "state-pill {state_cls}", "{state_text}" }
            span { class: "status-sep" }
            span { class: "status-label", "Epoch" }
            span { class: "status-value mono", "{epoch_label}" }
            span { class: "status-sep" }
            span { class: "status-label", "You" }
            span { class: "{role_cls}", "{role_text}" }
            span { class: "status-sep" }
            span { class: "status-label", "Members" }
            span { class: "status-value", "{member_count}" }
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
    let mut create_mode = use_signal(|| true);

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
    let cons = use_context::<Signal<ConsensusState>>();
    let session = use_context::<Signal<SessionState>>();
    let mut msg_input = use_signal(String::new);
    let mut show_ban_modal = use_signal(|| false);
    let mut ban_address = use_signal(String::new);
    let mut ban_error = use_signal(|| Option::<String>::None);

    // States where `send_app_message` refuses (matches the core guard in
    // `src/app/user/messaging.rs`). Keep these two lists in sync.
    let send_disabled = matches!(
        cons.read().group_state.as_str(),
        "PendingJoin" | "Freezing" | "Selection"
    );

    let send_msg = {
        move |_| {
            if send_disabled {
                return;
            }
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
                    div { class: "chat-actions",
                        button {
                            class: "ghost mini",
                            onclick: move |_| {
                                let group_id = gid.clone();
                                spawn(async move {
                                    let _ = GATEWAY
                                        .send(AppCmd::LeaveGroup { group_id })
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
            }
            if chat.read().opened_group.is_none() {
                div { class: "hint", "Pick a group to chat." }
            } else {
                ActiveVotesBanner {}
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
                StateContextStrip {}
                div { class: "composer",
                    input {
                        r#type: "text",
                        value: "{msg_input}",
                        oninput: move |e| msg_input.set(e.value()),
                        placeholder: if send_disabled { "Chat paused…" } else { "Type a message…" },
                        disabled: send_disabled,
                    }
                    button {
                        class: "primary",
                        onclick: send_msg,
                        disabled: send_disabled,
                        "Send"
                    }
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

// ─────────────────────────── Active votes banner ───────────────────────────

fn ActiveVotesBanner() -> Element {
    let chat = use_context::<Signal<ChatState>>();
    let mut cons = use_context::<Signal<ConsensusState>>();

    let opened = chat.read().opened_group.clone();
    let pending_votes: Vec<VotePayload> = cons
        .read()
        .pending_votes
        .iter()
        .filter(|p| Some(p.group_id.as_str()) == opened.as_deref())
        .cloned()
        .collect();

    if pending_votes.is_empty() {
        return rsx! {};
    }

    let mut do_vote = move |proposal_id: u32, choice: bool| {
        let vote = cons
            .read()
            .pending_votes
            .iter()
            .find(|v| v.proposal_id == proposal_id)
            .cloned();
        if let Some(v) = vote {
            cons.write()
                .pending_votes
                .retain(|p| p.proposal_id != proposal_id);
            spawn(async move {
                let _ = GATEWAY
                    .send(AppCmd::Vote {
                        group_id: v.group_id.clone(),
                        proposal_id: v.proposal_id,
                        choice,
                    })
                    .await;
            });
        }
    };

    let count = pending_votes.len();
    let title = if count == 1 {
        "⚡ Active vote".to_string()
    } else {
        format!("⚡ {count} active votes")
    };

    rsx! {
        div { class: "active-votes-banner",
            div { class: "votes-header", "{title}" }
            div { class: "votes-list",
                for vp in pending_votes.iter() {
                    {
                        let proposal_id = vp.proposal_id;
                        let (action, id) = GroupUpdateRequest::decode(vp.payload.as_slice())
                            .map(|req| format_group_request(&req))
                            .unwrap_or_else(|_| (
                                "Invalid".to_string(),
                                "malformed payload".to_string(),
                            ));
                        let is_emergency = action.starts_with("Emergency: ");
                        let is_election = action.starts_with("Steward Election");
                        let card_class = if is_emergency {
                            "vote-card emergency"
                        } else if is_election {
                            "vote-card election"
                        } else {
                            "vote-card"
                        };
                        let badge = if is_emergency {
                            let violation = action.strip_prefix("Emergency: ").unwrap_or("");
                            format!("⚠ Emergency · {violation}")
                        } else if is_election {
                            "⚡ Steward Election".to_string()
                        } else {
                            action.clone()
                        };

                        rsx! {
                            div { key: "{proposal_id}", class: "{card_class}",
                                div { class: "vote-card-head",
                                    span { class: "vote-badge", "{badge}" }
                                    span { class: "vote-id mono muted", "#{proposal_id}" }
                                }
                                {
                                    if is_election {
                                        let (meta, stewards) = parse_election_details(&id);
                                        rsx! {
                                            div { class: "vote-meta", "{meta}" }
                                            span { class: "vote-label", "Proposed stewards" }
                                            div { class: "vote-stewards-list",
                                                for s in stewards.iter() {
                                                    div { key: "{s}", class: "vote-steward mono", "{s}" }
                                                }
                                            }
                                        }
                                    } else if is_emergency {
                                        let target = fmt_addr(&id);
                                        rsx! {
                                            div { class: "vote-body-row",
                                                span { class: "vote-label", "Accused steward" }
                                                span { class: "vote-value mono", "{target}" }
                                            }
                                        }
                                    } else {
                                        let target = fmt_addr(&id);
                                        rsx! {
                                            div { class: "vote-body-row",
                                                span { class: "vote-label", "Target" }
                                                span { class: "vote-value mono", "{target}" }
                                            }
                                        }
                                    }
                                }
                                div { class: "vote-actions",
                                    button {
                                        class: "primary",
                                        onclick: move |_| do_vote(proposal_id, true),
                                        "YES"
                                    }
                                    button {
                                        class: "ghost",
                                        onclick: move |_| do_vote(proposal_id, false),
                                        "NO"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ─────────────────────────── State context strip ───────────────────────────

fn StateContextStrip() -> Element {
    let chat = use_context::<Signal<ChatState>>();
    let cons = use_context::<Signal<ConsensusState>>();

    if chat.read().opened_group.is_none() {
        return rsx! {};
    }

    let state = cons.read().group_state.clone();
    let (recv, exp) = cons.read().freeze_candidates;

    let (tone, message) = match state.as_str() {
        "Freezing" => (
            "warn",
            format!("⏳ Collecting commit candidates {recv}/{exp} — chat paused"),
        ),
        "Selection" => (
            "warn",
            "⚙ Applying selected commit — chat paused".to_string(),
        ),
        "Reelection" => (
            "bad",
            "⚠ Steward fault detected — waiting for emergency vote to resolve".to_string(),
        ),
        "PendingJoin" => (
            "warn",
            "⌛ Waiting for welcome message — chat disabled".to_string(),
        ),
        "Leaving" => (
            "warn",
            "⌛ Leave requested — waiting for removal commit".to_string(),
        ),
        _ => return rsx! {},
    };

    rsx! {
        div { class: "state-context-strip {tone}",
            span { "{message}" }
        }
    }
}

// ─────────────────────────── Right rail ───────────────────────────

fn RightRail() -> Element {
    let chat = use_context::<Signal<ChatState>>();
    let cons = use_context::<Signal<ConsensusState>>();
    let mut tab = use_context::<Signal<RailTab>>();

    if chat.read().opened_group.is_none() {
        return rsx! {
            div { class: "panel right-rail empty",
                div { class: "hint", "Open a group to see details." }
            }
        };
    }

    let current = *tab.read();
    let member_count = chat.read().members.len();
    let proposal_count = cons.read().approved_queue.len();

    let btn_class = |t: RailTab| {
        if current == t {
            "rail-tab active"
        } else {
            "rail-tab"
        }
    };

    rsx! {
        div { class: "panel right-rail",
            div { class: "rail-tabs",
                button {
                    class: btn_class(RailTab::Members),
                    onclick: move |_| tab.set(RailTab::Members),
                    span { "Members" }
                    if member_count > 0 {
                        span { class: "tab-count", "{member_count}" }
                    }
                }
                button {
                    class: btn_class(RailTab::Proposals),
                    onclick: move |_| tab.set(RailTab::Proposals),
                    span { "Proposals" }
                    if proposal_count > 0 {
                        span { class: "tab-count pending", "{proposal_count}" }
                    }
                }
                button {
                    class: btn_class(RailTab::History),
                    onclick: move |_| tab.set(RailTab::History),
                    span { "History" }
                }
            }
            div { class: "rail-content",
                {
                    match current {
                        RailTab::Members => rsx! { MembersTab {} },
                        RailTab::Proposals => rsx! { ProposalsTab {} },
                        RailTab::History => rsx! { HistoryTab {} },
                    }
                }
            }
        }
    }
}

fn MembersTab() -> Element {
    let chat = use_context::<Signal<ChatState>>();
    let members_snapshot = chat.read().members.clone();
    let removal_threshold = 0_i64;

    if members_snapshot.is_empty() {
        return rsx! { div { class: "no-data", "No members loaded." } };
    }

    rsx! {
        div { class: "tab-pane members-tab",
            div { class: "member-table",
                div { class: "member-row header",
                    span { class: "col-addr", "Address" }
                    span { class: "col-role", "Role" }
                    span { class: "col-score", "Score" }
                }
                div { class: "member-rows",
                    for member in members_snapshot.iter() {
                        {
                            let (role_cls, role_txt) = role_for(member.role.as_str());
                            let score_class = if member.score <= removal_threshold {
                                "col-score score bad"
                            } else if member.score <= removal_threshold + 30 {
                                "col-score score warn"
                            } else {
                                "col-score score"
                            };
                            let short = fmt_addr(&member.address);
                            let row_class = if member.pending_leave {
                                "member-row leaving"
                            } else {
                                "member-row"
                            };
                            rsx! {
                                div { class: "{row_class}",
                                    span { class: "col-addr mono", "{short}" }
                                    span { class: "col-role {role_cls}", "{role_txt}" }
                                    span { class: "{score_class}", "{member.score}" }
                                    if member.pending_leave {
                                        span { class: "leave-badge", title: "Leaving next epoch", "⤴ leaving" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn ProposalsTab() -> Element {
    let cons = use_context::<Signal<ConsensusState>>();
    let state = cons.read().group_state.clone();
    let is_steward = cons.read().is_steward;
    let approved = cons.read().approved_queue.clone();
    let n = approved.len();

    let header = if state == "Working" {
        if is_steward {
            format!("Approved for next commit ({n})")
        } else if n > 0 {
            let suffix = if n == 1 { "" } else { "s" };
            format!("Waiting for steward to commit {n} proposal{suffix}")
        } else {
            "No pending proposals".to_string()
        }
    } else {
        format!("Approved queue ({n})")
    };

    rsx! {
        div { class: "tab-pane proposals-tab",
            div { class: "tab-header", "{header}" }
            if n == 0 {
                div { class: "no-data", "Nothing queued" }
            } else {
                div { class: "proposal-list",
                    for (action, address) in approved.iter() {
                        {
                            let is_emerg = action.contains("Emergency");
                            let card_class = if is_emerg {
                                "proposal-card emergency"
                            } else {
                                "proposal-card"
                            };
                            let short = fmt_addr(address);
                            rsx! {
                                div { class: "{card_class}",
                                    span { class: "proposal-action", "{action}" }
                                    span { class: "proposal-target mono", "{short}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn HistoryTab() -> Element {
    let cons = use_context::<Signal<ConsensusState>>();
    let rejected_snapshot = cons.read().rejected.clone();
    let epoch_snapshot = cons.read().epoch_history.clone();
    let elections_snapshot = cons.read().elections.clone();
    let epoch_count = epoch_snapshot.len();

    let any = !rejected_snapshot.is_empty()
        || !epoch_snapshot.is_empty()
        || !elections_snapshot.is_empty();
    if !any {
        return rsx! { div { class: "no-data", "No history yet" } };
    }

    rsx! {
        div { class: "tab-pane history-tab",
            if !elections_snapshot.is_empty() {
                div { class: "history-group",
                    span { class: "history-label", "Steward Elections" }
                    for er in elections_snapshot.iter().rev() {
                        {
                            let (entry_class, outcome_label, outcome_class) = if er.accepted {
                                ("history-card applied election", "applied", "good")
                            } else {
                                ("history-card rejected election", "rejected", "bad")
                            };
                            let show_reason = er.reason.is_some_and(|r| r != "Rejected");
                            let (header_part, stewards_part) = er
                                .details
                                .split_once(" | ")
                                .unwrap_or((er.details.as_str(), ""));
                            rsx! {
                                div { class: "{entry_class}",
                                    div { class: "history-card-head",
                                        span { class: "history-card-title", "Election" }
                                        span { class: "history-outcome {outcome_class}", "{outcome_label}" }
                                        if let Some(reason) = er.reason {
                                            if show_reason {
                                                span { class: "history-reason muted", "({reason})" }
                                            }
                                        }
                                    }
                                    div { class: "history-card-body",
                                        span { class: "value mono", "{header_part}" }
                                        if !stewards_part.is_empty() {
                                            span { class: "election-stewards mono muted", "{stewards_part}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !rejected_snapshot.is_empty() {
                div { class: "history-group",
                    span { class: "history-label", "Rejected" }
                    for rp in rejected_snapshot.iter().rev() {
                        {
                            let short = fmt_addr(&rp.address);
                            let reason = rp.reason;
                            let show_reason = reason != "Rejected";
                            rsx! {
                                div { class: "history-card rejected",
                                    div { class: "history-card-head",
                                        span { class: "history-card-title", "{rp.action}" }
                                        span { class: "history-outcome bad", "rejected" }
                                        if show_reason {
                                            span { class: "history-reason muted", "({reason})" }
                                        }
                                    }
                                    div { class: "history-card-body",
                                        span { class: "value mono", "{short}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !epoch_snapshot.is_empty() {
                div { class: "history-group",
                    span { class: "history-label", "Past Epochs" }
                    for (i, batch) in epoch_snapshot.iter().rev().enumerate() {
                        div { class: "epoch-group",
                            span { class: "epoch-label", "Epoch {epoch_count - i}" }
                            for (action, address) in batch.iter() {
                                {
                                    let is_emerg = action.contains("Emergency");
                                    let entry_class = if is_emerg {
                                        "history-card applied emergency"
                                    } else {
                                        "history-card applied"
                                    };
                                    let display_action = if is_emerg {
                                        format!("⚠ {}", action)
                                    } else {
                                        action.clone()
                                    };
                                    let short = fmt_addr(address);
                                    rsx! {
                                        div { class: "{entry_class}",
                                            div { class: "history-card-head",
                                                span { class: "history-card-title", "{display_action}" }
                                                span { class: "history-outcome good", "applied" }
                                            }
                                            div { class: "history-card-body",
                                                span { class: "value mono", "{short}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
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
