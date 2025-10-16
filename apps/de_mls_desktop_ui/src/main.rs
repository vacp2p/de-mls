// apps/de_mls_desktop_ui/src/main.rs
#![allow(non_snake_case)]

use de_mls::message::convert_group_requests_to_display;
use dioxus::prelude::*;
use dioxus_desktop::{launch::launch as desktop_launch, Config, LogicalSize, WindowBuilder};

use de_mls::protos::consensus::v1::{Outcome, VotePayload};
use de_mls::protos::de_mls::messages::v1::ConversationMessage;
use de_mls::{bootstrap::bootstrap_core_from_env, protos::consensus::v1::ProposalResult};
use de_mls_gateway::GATEWAY;
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};

mod logging;

// Helper function to format timestamps
fn format_timestamp(timestamp_ms: u64) -> String {
    use std::time::UNIX_EPOCH;

    // Convert to SystemTime and format
    let timestamp = UNIX_EPOCH + std::time::Duration::from_secs(timestamp_ms);
    let datetime: chrono::DateTime<chrono::Utc> = timestamp.into();
    datetime.format("%H:%M:%S").to_string()
}

// ─────────────────────────── App state ───────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
struct SessionState {
    name: Option<String>,
    key: Option<String>,
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
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ConsensusState {
    is_steward: bool,
    pending: Option<VotePayload>, // active/pending proposal for opened group
    // Store results with timestamps for better display
    latest_results: Vec<(u32, Outcome, u64)>, // (vote_id, result, timestamp_ms)
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
    use_context_provider(|| Signal::new(SessionState::default()));
    use_context_provider(|| Signal::new(GroupsState::default()));
    use_context_provider(|| Signal::new(ChatState::default()));
    use_context_provider(|| Signal::new(ConsensusState::default()));

    rsx! {
        style { {CSS} }
        HeaderBar {}
        Router::<Route> {}
    }
}

fn HeaderBar() -> Element {
    // local signal to reflect current level in the select
    let level = use_signal(|| std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()));

    let on_change = {
        let mut level = level.clone();
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
    let mut sess = use_context::<Signal<SessionState>>();
    let key = use_signal(|| String::new());

    // Local single-consumer loop: only Login() steals LoggedIn events
    use_future({
        let nav = nav.clone();
        let mut sess = sess.clone();
        move || async move {
            loop {
                match GATEWAY.next_event().await {
                    Some(AppEvent::LoggedIn(name)) => {
                        sess.write().name = Some(name);
                        nav.replace(Route::Home);
                        break;
                    }
                    _ => {}
                }
            }
        }
    });

    let oninput_key = {
        let mut key = key.clone();
        move |e: FormEvent| key.set(e.value())
    };

    let mut on_submit = move |_| {
        let k = key.read().trim().to_string();
        if k.is_empty() {
            return;
        }
        sess.write().key = Some(k.clone());
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
    let groups = use_context::<Signal<GroupsState>>();
    let chat = use_context::<Signal<ChatState>>();
    let cons = use_context::<Signal<ConsensusState>>();

    // Ask backend once
    use_future({
        let groups = groups.clone();
        move || async move {
            if !groups.read().loaded {
                let _ = GATEWAY.send(AppCmd::ListGroups).await;
            }
        }
    });

    // Local event loop for: Groups, ChatMessage, VoteRequested, VoteClosed
    use_future({
        let mut groups = groups.clone();
        let mut chat = chat.clone();
        let mut cons = cons.clone();
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
                    Some(AppEvent::ProposalDecided(ProposalResult {
                        group_id,
                        proposal_id,
                        outcome,
                        decided_at_ms,
                    })) => {
                        // store for the opened group (or keep a map per group if you prefer)
                        if chat.read().opened_group.as_deref() == Some(group_id.as_str()) {
                            cons.write().latest_results.push((
                                proposal_id.clone(),
                                Outcome::try_from(outcome).unwrap_or(Outcome::Unspecified),
                                decided_at_ms,
                            ));
                        }
                        cons.write().pending = None;
                    }
                    Some(AppEvent::GroupRemoved(name)) => {
                        // keep in sync if backend asks us to remove
                        let mut g = groups.write();
                        g.items.retain(|n| n != &name);
                        if chat.read().opened_group.as_deref() == Some(name.as_str()) {
                            chat.write().opened_group = None;
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    rsx! {
        div { class: "page home",
            // 3 columns
            div { class: "layout",
                // 1) Group list
                GroupListSection {}
                // 2) Chat
                ChatSection {}
                // 3) Consensus
                ConsensusSection {}
            }
        }
    }
}

// ─────────────────────────── Sections ───────────────────────────

fn GroupListSection() -> Element {
    let groups_state = use_context::<Signal<GroupsState>>();
    let mut chat = use_context::<Signal<ChatState>>();
    let mut show_modal = use_signal(|| false);
    let mut new_name = use_signal(|| String::new());
    let mut create_mode = use_signal(|| true); // true=create, false=join

    // Take a snapshot so RSX/closures don't borrow `groups_state` for 'static.
    let items_snapshot: Vec<String> = groups_state.read().items.clone();
    let loaded = groups_state.read().loaded;

    let mut open_group = {
        move |name: String| {
            chat.write().opened_group = Some(name.clone());
            let gname = name.clone();
            spawn(async move {
                let _ = GATEWAY
                    .send(AppCmd::EnterGroup {
                        group_id: gname.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::LoadHistory {
                        group_id: gname.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::QuerySteward {
                        group_id: gname.clone(),
                    })
                    .await;
            });
        }
    };

    let mut modal_submit = {
        let mut show_modal = show_modal.clone();
        let mut new_name = new_name.clone();
        let create_mode = create_mode.clone();
        let mut open_group = open_group;

        move |_| {
            let name = new_name.read().trim().to_string();
            if name.is_empty() {
                return;
            }
            // clone name for async action so we can still use `name` locally
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
            // Immediately open the group in the UI
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
                        // let name_for_btn = name.clone();
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
    let sess = use_context::<Signal<SessionState>>();
    let mut msg_input = use_signal(|| String::new());

    let send_msg = {
        let mut msg_input = msg_input.clone();
        let chat = chat.clone();
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

    // Only render messages for the opened group
    let msgs_for_group = {
        let opened = chat.read().opened_group.clone();
        chat.read()
            .messages
            .iter()
            .filter(|m| Some(m.group_name.as_str()) == opened.as_deref())
            .cloned()
            .collect::<Vec<_>>()
    };

    let my_name = sess.read().name.clone();

    rsx! {
        div { class: "panel chat",
            h2 { "Chat" }
            if chat.read().opened_group.is_none() {
                div { class: "hint", "Pick a group to chat." }
            } else {
                div { class: "messages",
                    for (i, m) in msgs_for_group.iter().enumerate() {
                        if my_name.as_deref() == Some(m.sender.as_str()) || m.sender.eq_ignore_ascii_case("me") {
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
    }
}

fn ConsensusSection() -> Element {
    let chat = use_context::<Signal<ChatState>>();
    let cons = use_context::<Signal<ConsensusState>>();

    let vote_yes = {
        let cons = cons.clone();
        move |_| {
            if let Some(v) = cons.read().pending.clone() {
                spawn(async move {
                    let _ = GATEWAY
                        .send(AppCmd::Vote {
                            group_id: v.group_id.clone(),
                            proposal_id: v.proposal_id.clone(),
                            choice: true,
                        })
                        .await;
                });
            }
        }
    };
    let vote_no = {
        let cons = cons.clone();
        move |_| {
            if let Some(v) = cons.read().pending.clone() {
                spawn(async move {
                    let _ = GATEWAY
                        .send(AppCmd::Vote {
                            group_id: v.group_id.clone(),
                            proposal_id: v.proposal_id.clone(),
                            choice: false,
                        })
                        .await;
                });
            }
        }
    };

    // active only when we have an opened group and a pending proposal for it
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

                // Current proposal block
                if let Some(v) = pending {
                    div { class: "proposal",
                        div { class: "proposal-header",
                            span { class: "proposal-label", "Proposal:" }
                            span { class: "proposal-id", "{v.proposal_id}" }
                        }
                        div { class: "proposal-payload",
                            span { class: "payload-label", "Payload:" }
                            div { class: "payload-items",
                                for (action, id) in convert_group_requests_to_display(&v.group_requests) {
                                    div { class: "payload-item",
                                        span { class: "action", "{action}:" }
                                        span { class: "id", "{id}" }
                                    }
                                }
                            }
                        }

                        // Buttons — active now; when you have epochs, gate here.
                        div { class: "actions",
                            button { class: "primary", onclick: vote_yes, "YES" }
                            button { class: "ghost",   onclick: vote_no,  "NO"  }
                        }
                    }
                } else {
                    div { class: "hint", "No active proposal." }
                }

                // Latest results
                if cons.read().latest_results.is_empty() {
                    div { class: "hint small", "No decisions yet." }
                } else {
                    div { class: "results",
                        h3 { "Latest Consensus Results" }
                        div { class: "results-window",
                            for (vid, res, timestamp_ms) in cons.read().latest_results.iter().rev() {
                                div { class: "result-item",
                                    span { class: "proposal-id", "{vid}" }
                                    span {
                                        class: match res {
                                            Outcome::Accepted => "outcome accepted",
                                            Outcome::Rejected => "outcome rejected",
                                            Outcome::Unspecified => "outcome unspecified",
                                        },
                                        match res {
                                            Outcome::Accepted => "Accepted",
                                            Outcome::Rejected => "Rejected",
                                            Outcome::Unspecified => "Unspecified",
                                        }
                                    }
                                    span { class: "timestamp",
                                        "{format_timestamp(*timestamp_ms)}"
                                    }
                                }
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

// ─────────────────────────── Minimal CSS ───────────────────────────

const CSS: &str = r#"
:root {
  --bg: #0e0f12;
  --card: #17191e;
  --text: #e8e9ec;
  --muted: #a3a7b3;
  --primary: #4f8cff;
  --primary-2: #3b6ad1;
  --border: #23262d;
  --good: #17c964;
  --bad: #f31260;
}

* { box-sizing: border-box; }
html, body, #main { height: 100%; width: 100%; margin: 0; padding: 0; background: var(--bg); color: var(--text); font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial, Noto Sans, Apple Color Emoji, Segoe UI Emoji; }
a { color: var(--primary); text-decoration: none; }

.page { max-width: 1400px; margin: 0 auto; }
h1 { margin: 0 0 16px 0; font-size: 22px; }

.header {
  display: flex; align-items: center; gap: 12px;
  padding: 8px 12px; border-bottom: 1px solid var(--border);
  background: rgba(255,255,255,0.03); position: sticky; top: 0; z-index: 5;
}
.header .brand { font-weight: 700; letter-spacing: .5px; }
.header .spacer { flex: 1; }
.header .label { color: var(--muted); }
.header .level {
  padding: 6px 8px; border-radius: 8px; border: 1px solid var(--border);
  background: var(--card); color: var(--text); outline: none; font-size: 13px;
}

.page.login { max-width: 520px; margin-top: 32px; }
.form-row { display: flex; flex-direction: column; gap: 6px; margin: 12px 0; }
input, select {
  padding: 10px 12px; border-radius: 8px; border: 1px solid var(--border);
  background: var(--card); color: var(--text); outline: none; font-size: 14px;
}
button {
  border: 1px solid var(--border); background: var(--card); color: var(--text);
  padding: 10px 14px; border-radius: 10px; cursor: pointer;
}
button.primary { background: var(--primary); border-color: var(--primary); color: white; }
button.primary:hover { background: var(--primary-2); }
button.secondary { background: transparent; }
button.ghost { background: transparent; border-color: var(--border); color: var(--muted); }
button.icon { width: 32px; height: 32px; border-radius: 8px; }
button.mini { padding: 4px 8px; border-radius: 8px; font-size: 12px; }

/* Home layout */
.page.home { padding: 12px; }
.layout {
  display: grid;
  grid-template-columns: 280px 1fr 460px;
  gap: 12px;
}

.mono { font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace; }
.ellipsis { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }


.panel {
  background: var(--card); border: 1px solid var(--border); border-radius: 12px;
  padding: 12px; display: flex; flex-direction: column; gap: 10px;
}
.panel h2 { margin: 0 0 6px 0; font-size: 18px; }
.hint { color: var(--muted); padding: 8px 0; }
.hint.small { font-size: 12px; }

.panel.groups .group-list { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 8px; }
/* Group list a bit wider rows to align with long names */
.group-row { display: flex; align-items: center; justify-content: space-between;
  padding: 10px 12px; border: 1px solid var(--border); border-radius: 10px; }
.group-row .title { font-weight: 600; max-width: 220px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.panel.groups .footer { margin-top: auto; display: flex; justify-content: flex-end; }
.panel.groups .footer { gap: 8px; }
.panel.groups .footer .primary { flex: 1; }

/* Chat */
.panel.chat .messages {
  min-height: 360px; height: 58vh; overflow-y: auto; border: 1px solid var(--border);
  border-radius: 12px; padding: 12px; display: flex; flex-direction: column; gap: 10px;
}
.msg { display: flex; flex-direction: column; gap: 4px; align-items: flex-start; }
.msg.me { align-items: flex-end; }
.msg.me .body { background: rgba(79,140,255,0.15); border: 1px solid rgba(79,140,255,0.35); padding: 8px 10px; border-radius: 10px; }
.msg.system { opacity: 0.9; }
.msg.system .body { font-style: italic; color: var(--muted); background: transparent; border: none; padding: 0; }
.msg .from { color: var(--muted); font-size: 16px; }
.msg .body { color: var(--text); background: rgba(255,255,255,0.05); border: 1px solid var(--border); padding: 8px 10px; border-radius: 10px; }
.composer { display: flex; gap: 8px; align-items: center; }
.composer input { flex: 1; min-width: 0; }
.composer button { flex: 0 0 auto; }

/* Consensus panel */
.panel.consensus .status { display: flex; align-items: center; gap: 8px; }
.panel.consensus .status .good { color: var(--good); font-weight: 600; }
.panel.consensus .status .bad  { color: var(--bad);  font-weight: 600; }

.panel.consensus .proposal {
  border: 1px solid var(--border); border-radius: 10px; padding: 12px;
  display: flex; flex-direction: column; gap: 12px;
}
.panel.consensus .proposal-header {
  display: flex; align-items: center; gap: 8px;
  padding-bottom: 8px; border-bottom: 1px solid var(--border);
}
.panel.consensus .proposal-label {
  color: var(--muted); font-weight: 600; font-size: 14px;
}
.panel.consensus .proposal-id {
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
  color: var(--text); font-weight: 600; font-size: 14px;
}
.panel.consensus .proposal-payload {
  display: flex; flex-direction: column; gap: 8px;
}
.panel.consensus .payload-label {
  color: var(--muted); font-weight: 600; font-size: 13px;
}
.panel.consensus .payload-items {
  display: flex; flex-direction: column; gap: 6px;
}
.panel.consensus .payload-item {
  display: flex; align-items: center; gap: 8px;
  padding: 6px 8px; border-radius: 6px; background: rgba(255,255,255,0.03);
  border: 1px solid var(--border);
}
.panel.consensus .payload-item .action {
    font-size: 12px; color: var(--primary); min-width: 60px;
}
.panel.consensus .payload-item .id {
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
  color: var(--text); font-size: 12px; word-break: break-all;
}
.panel.consensus .actions { display: flex; gap: 8px; justify-content: flex-end; margin-top: 6px; }

/* Consensus results window */
.panel.consensus .results-window {
  max-height: 200px; overflow-y: auto; border: 1px solid var(--border);
  border-radius: 8px; padding: 8px; background: rgba(255,255,255,0.02);
  display: flex; flex-direction: column; gap: 6px;
}
.panel.consensus .result-item {
  display: flex; justify-content: space-between; align-items: center;
  padding: 6px 8px; border-radius: 6px; background: rgba(255,255,255,0.03);
  border: 1px solid var(--border); font-size: 13px;
}
.panel.consensus .result-item .proposal-id {
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
  color: var(--muted); font-weight: 600;
}
.panel.consensus .result-item .outcome {
  font-weight: 600; padding: 2px 6px; border-radius: 4px;
}
.panel.consensus .result-item .outcome.accepted { color: var(--good); background: rgba(23,201,100,0.1); }
.panel.consensus .result-item .outcome.rejected { color: var(--bad); background: rgba(243,18,96,0.1); }
.panel.consensus .result-item .outcome.unspecified { color: var(--muted); background: rgba(163,167,179,0.1); }
.panel.consensus .result-item .timestamp {
  color: var(--muted); font-size: 11px; font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
}

/* Modal */
.modal-backdrop {
  position: fixed; inset: 0; background: rgba(0,0,0,.45);
  display: flex; align-items: center; justify-content: center;
}
.modal {
  width: 520px; max-width: calc(100vw - 32px);
  background: var(--card); border: 1px solid var(--border); border-radius: 14px;
  box-shadow: 0 16px 48px rgba(0,0,0,.4);
}
.modal-head {
  display: flex; align-items: center; justify-content: space-between;
  padding: 12px 14px; border-bottom: 1px solid var(--border);
}
.modal-body { padding: 14px; display: flex; flex-direction: column; gap: 10px; }
.actions { display: flex; gap: 8px; justify-content: flex-end; margin-top: 6px; }
"#;
