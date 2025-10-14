#![allow(non_snake_case)]

use dioxus::prelude::*;
use dioxus_desktop::{launch::launch as desktop_launch, Config, LogicalSize, WindowBuilder};

use de_mls::bootstrap::bootstrap_core_from_env;
use de_mls_gateway::GATEWAY;
use de_mls_ui_protocol::v1::{AppCmd, AppEvent, ChatMsg, VotePayload};

mod logging;

// ─────────────────────────── App state ───────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
struct SessionState {
    name: Option<String>,
    key: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct GroupsState {
    items: Vec<String>, // <- names only
    loaded: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ChatState {
    group_id: Option<String>,
    messages: Vec<ChatMsg>,
    active_vote: Option<VotePayload>,
}

// ─────────────────────────── Routing ───────────────────────────

#[derive(Routable, Clone, PartialEq)]
enum Route {
    #[route("/")]
    Login,
    #[route("/groups")]
    Groups,
    #[route("/chat/:group_id")]
    Chat { group_id: String },
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
            .with_inner_size(LogicalSize::new(800, 600))
            .with_resizable(true),
    );

    tracing::info!("Launching desktop application");
    desktop_launch(App, vec![], vec![Box::new(config)]);
}

fn App() -> Element {
    use_context_provider(|| Signal::new(SessionState::default()));
    use_context_provider(|| Signal::new(GroupsState::default()));
    use_context_provider(|| Signal::new(ChatState::default()));

    rsx! {
        style { {CSS} }
        HeaderBar {}
        Router::<Route> {}
    }
}

fn HeaderBar() -> Element {
    tracing::info!("HeaderBar component rendered");
    // local signal to reflect current level in the select
    let level = use_signal(|| std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()));

    let on_change = {
        let mut level = level.clone();
        move |evt: FormEvent| {
            let new_val = evt.value();
            // apply live
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
                        nav.replace(Route::Groups);
                        break; // leave loop after navigating
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
            let _ = GATEWAY.start().await;
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

fn Groups() -> Element {
    let nav = use_navigator();
    let groups_state = use_context::<Signal<GroupsState>>();
    let mut show_modal = use_signal(|| false);
    let mut new_name = use_signal(|| String::new());
    let mut create_mode = use_signal(|| true); // true=create, false=join

    // Ask backend once
    use_future({
        let groups_state = groups_state.clone();
        move || async move {
            if !groups_state.read().loaded {
                let _ = GATEWAY.start().await;
                let _ = GATEWAY.send(AppCmd::ListGroups).await;
            }
        }
    });
    // Local single-consumer loop: only Groups() listens for groups-related events
    use_future({
        let mut groups_state = groups_state.clone();
        move || async move {
            loop {
                match GATEWAY.next_event().await {
                    Some(AppEvent::Groups(names)) => {
                        groups_state.write().items = names;
                        groups_state.write().loaded = true;
                    }
                    Some(AppEvent::GroupCreated(name)) => {
                        let mut st = groups_state.write();
                        if !st.items.iter().any(|g| g == &name) {
                            st.items.push(name);
                        }
                    }
                    Some(AppEvent::GroupRemoved(name)) => {
                        let mut st = groups_state.write();
                        st.items.retain(|g| g != &name);
                    }
                    _ => {}
                }
            }
        }
    });

    let mut modal_submit = {
        let mut show_modal = show_modal.clone();
        let mut new_name = new_name.clone();
        let create_mode = create_mode.clone();
        let mut groups_state = groups_state.clone();

        move |_| {
            let name = new_name.read().trim().to_string();
            if name.is_empty() {
                return;
            }

            groups_state.write().loaded = false;

            if *create_mode.read() {
                spawn(async move {
                    tracing::debug!("Creating group: {}", name);
                    let _ = GATEWAY
                        .send(AppCmd::CreateGroup { name: name.clone() })
                        .await;
                    let _ = GATEWAY.send(AppCmd::ListGroups).await;
                });
            } else {
                spawn(async move {
                    let _ = GATEWAY.send(AppCmd::JoinGroup { name: name.clone() }).await;
                    let _ = GATEWAY.send(AppCmd::ListGroups).await;
                });
            }

            new_name.set(String::new());
            show_modal.set(false);
        }
    };

    rsx! {
        div { class: "page groups",
            h1 { "Groups" }

            div { class: "toolbar",
                button { class: "primary", onclick: move |_| { show_modal.set(true); }, "New / Join" }
            }

            if !groups_state.read().loaded {
                div { class: "hint", "Loading groups…" }
            } else if groups_state.read().items.is_empty() {
                div { class: "hint", "No groups yet." }
            } else {
                ul { class: "group-list",
                    for name in groups_state.read().items.iter() {
                        li {
                            key: "{name}",
                            class: "group-row",
                            div { class: "title", "{name}" }
                            button {
                                class: "secondary",
                                onclick: {
                                    let group = name.clone();
                                    move |_| { nav.push(Route::Chat { group_id: group.clone() }); }
                                },
                                "Open"
                            }
                        }
                    }
                }
            }

            if *show_modal.read() {
                Modal {
                    title: "Create / Join Group".to_string(),
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
                    div { class: "form-row",
                        label { "Mode" }
                        select {
                            value: if *create_mode.read() { "create" } else { "join" },
                            oninput: move |e| create_mode.set(e.value() == "create"),
                            option { value: "create", "Create" }
                            option { value: "join",   "Join" }
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

#[component]
fn Chat(group_id: String) -> Element {
    let mut chat_state = use_context::<Signal<ChatState>>();
    let mut msg_input = use_signal(|| String::new());

    // Enter room + ask history once
    use_future({
        let mut chat_state = chat_state.clone();
        let gid = group_id.clone();
        move || {
            let gid = gid.clone();
            async move {
                chat_state.write().group_id = Some(gid.clone());
                let _ = GATEWAY
                    .send(AppCmd::EnterGroup {
                        group_id: gid.clone(),
                    })
                    .await;
                let _ = GATEWAY
                    .send(AppCmd::LoadHistory {
                        group_id: gid.clone(),
                    })
                    .await;
            }
        }
    });

    // Local single-consumer loop: only Chat() listens to chat-related events
    use_future({
        let mut chat_state = chat_state.clone();
        move || async move {
            loop {
                match GATEWAY.next_event().await {
                    Some(AppEvent::ChatMessage(msg)) => {
                        chat_state.write().messages.push(msg);
                    }
                    Some(AppEvent::VoteRequested(vote)) => {
                        chat_state.write().active_vote = Some(vote);
                    }
                    Some(AppEvent::VoteClosed { .. }) => {
                        chat_state.write().active_vote = None;
                    }
                    _ => {}
                }
            }
        }
    });

    // Handlers
    let send_msg = {
        let gid = group_id.clone();
        let mut msg_input = msg_input.clone();
        move |_| {
            let text = msg_input.read().trim().to_string();
            if text.is_empty() {
                return;
            }
            msg_input.set(String::new());

            let gid2 = gid.clone();
            let text2 = text.clone();
            spawn(async move {
                let _ = GATEWAY
                    .send(AppCmd::SendMessage {
                        group_id: gid2,
                        body: text2,
                    })
                    .await;
            });
        }
    };

    let vote_yes = {
        let chat_state = chat_state.clone();
        move |_| {
            if let Some(v) = chat_state.read().active_vote.clone() {
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
        let chat_state = chat_state.clone();
        move |_| {
            if let Some(v) = chat_state.read().active_vote.clone() {
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

    rsx! {
        div { class: "page chat",
            h1 { "Group: {group_id}" }

            div { class: "messages",
                for (i, m) in chat_state.read().messages.iter().enumerate() {
                    div { key: "{i}", class: "msg",
                        span { class: "from", "{m.author}" }
                        span { class: "body", "{m.body}" }
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

            if let Some(v) = chat_state.read().active_vote.as_ref() {
                Modal {
                    title: "Vote".to_string(),
                    on_close: move || { chat_state.write().active_vote = None; },
                    p { "{v.proposal_id}" }
                    div { class: "actions",
                        button { class: "primary", onclick: vote_yes, "YES" }
                        button { class: "ghost",   onclick: vote_no,  "NO" }
                    }
                }
            }
        }
    }
}

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
}

* { box-sizing: border-box; }
html, body, #main { height: 100%; width: 100%; margin: 0; padding: 0; background: var(--bg); color: var(--text); font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial, Noto Sans, Apple Color Emoji, Segoe UI Emoji; }
a { color: var(--primary); text-decoration: none; }

.page { max-width: 960px; margin: 24px auto; padding: 12px; }
h1 { margin: 0 0 16px 0; font-size: 22px; }

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

.toolbar { display: flex; justify-content: flex-end; margin-bottom: 12px; }
.hint { color: var(--muted); padding: 16px 0; }

ul.group-list { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 8px; }
.group-row {
  display: flex; align-items: center; justify-content: space-between;
  background: var(--card); padding: 12px 14px; border-radius: 12px; border: 1px solid var(--border);
}
.group-row .title { font-weight: 600; }

.page.chat .messages {
  height: 60vh; overflow-y: auto; border: 1px solid var(--border);
  background: var(--card); border-radius: 12px; padding: 12px; display: flex; flex-direction: column; gap: 10px;
}
.msg { display: flex; gap: 8px; }
.msg .from { color: var(--muted); min-width: 120px; }
.msg .body { color: var(--text); }

.composer { display: flex; gap: 8px; margin-top: 12px; }

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
"#;
