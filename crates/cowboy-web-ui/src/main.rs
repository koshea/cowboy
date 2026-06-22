//! cowboy web UI — a Yew single-page app that attaches to a live agent session
//! over a WebSocket. It is the "fat client" half of `cowboy web`: the server is a
//! transparent relay, and this app does all the rendering by replaying the same
//! `ServerMsg`/`UiEventMsg` stream the TUI consumes.

mod model;

use std::cell::RefCell;
use std::rc::Rc;

use cowboy_proto::daemonproto::{ClientMsg, InterruptKind, ServerMsg, SessionInfo, SessionStatus};
use cowboy_proto::netproto::{ApprovalScope, Verdict};
use futures::channel::mpsc;
use futures::{FutureExt, SinkExt, StreamExt};
use gloo_net::http::Request;
use gloo_net::websocket::{futures::WebSocket, Message as WsMessage};
use web_sys::{Event, HtmlTextAreaElement, KeyboardEvent};
use yew::prelude::*;

use model::{Block, Model};

/// Reducer actions for the session [`Model`].
pub enum Action {
    Server(ServerMsg),
    /// Optimistic local echo of a message the user just sent.
    User(String),
    /// A fresh WebSocket opened.
    Connected,
    /// The WebSocket dropped (transient) — retrying.
    Disconnected,
}

impl Reducible for Model {
    type Action = Action;
    fn reduce(self: Rc<Self>, action: Action) -> Rc<Self> {
        let mut m = (*self).clone();
        match action {
            Action::Server(msg) => m.apply(msg),
            Action::User(text) => m.push_user(text),
            Action::Connected => m.set_live(),
            Action::Disconnected => m.set_reconnecting(),
        }
        Rc::new(m)
    }
}

/// Read `?key=` from the page URL.
fn query_param(key: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let q = search.trim_start_matches('?');
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| js_decode(v))
    })
}

/// Minimal percent-decode (tokens are hex UUIDs, but ids may contain `-`).
fn js_decode(s: &str) -> String {
    js_sys::decode_uri_component(s)
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| s.to_string())
}

#[function_component(App)]
fn app() -> Html {
    let token = use_state(|| query_param("token").unwrap_or_default());
    // Selected session id: from `?session=` or chosen from the list.
    let selected = use_state(|| query_param("session"));

    if token.is_empty() {
        return html! {
            <div class="center">
                <h1>{ "cowboy" }</h1>
                <p class="muted">{ "Missing access token. Open the URL printed by " }<code>{ "cowboy web" }</code>{ "." }</p>
            </div>
        };
    }

    match (*selected).clone() {
        Some(id) => {
            let back = {
                let selected = selected.clone();
                Callback::from(move |_| selected.set(None))
            };
            html! { <Session id={id} token={(*token).clone()} on_back={back} /> }
        }
        None => {
            let pick = {
                let selected = selected.clone();
                Callback::from(move |id: String| selected.set(Some(id)))
            };
            html! { <SessionList token={(*token).clone()} on_pick={pick} /> }
        }
    }
}

#[derive(Properties, PartialEq)]
struct ListProps {
    token: String,
    on_pick: Callback<String>,
}

#[function_component(SessionList)]
fn session_list(props: &ListProps) -> Html {
    let sessions = use_state(Vec::<SessionInfo>::new);
    let error = use_state(|| Option::<String>::None);

    {
        let sessions = sessions.clone();
        let error = error.clone();
        let token = props.token.clone();
        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                match Request::get("/api/sessions")
                    .header("Authorization", &format!("Bearer {token}"))
                    .send()
                    .await
                {
                    Ok(resp) if resp.ok() => match resp.json::<Vec<SessionInfo>>().await {
                        Ok(list) => sessions.set(list),
                        Err(e) => error.set(Some(format!("bad response: {e}"))),
                    },
                    Ok(resp) => error.set(Some(format!("server error {}", resp.status()))),
                    Err(e) => error.set(Some(format!("request failed: {e}"))),
                }
            });
            || ()
        });
    }

    let rows = sessions.iter().map(|s| {
        let id = s.id.clone();
        let pick = props.on_pick.clone();
        let onclick = Callback::from(move |_| pick.emit(id.clone()));
        let task = s.task.clone().unwrap_or_else(|| "(no task)".into());
        html! {
            <li class="session-row" {onclick}>
                <div class="session-task">{ task }</div>
                <div class="session-meta muted">
                    { status_label(&s.status) }{ " · " }{ s.id.clone() }
                </div>
            </li>
        }
    });

    html! {
        <div class="page">
            <header class="bar"><h1>{ "cowboy sessions" }</h1></header>
            if let Some(e) = (*error).clone() {
                <p class="error">{ e }</p>
            }
            <ul class="session-list">{ for rows }</ul>
            if sessions.is_empty() && error.is_none() {
                <p class="muted center">{ "No sessions. Start one with " }<code>{ "cowboy \"…\"" }</code>{ "." }</p>
            }
        </div>
    }
}

#[derive(Properties, PartialEq)]
struct SessionProps {
    id: String,
    token: String,
    on_back: Callback<()>,
}

#[function_component(Session)]
fn session(props: &SessionProps) -> Html {
    let model = use_reducer(Model::default);
    // Outbound channel: handlers push ClientMsg; the WS task drains it. Created
    // once so the sender is stable across renders.
    let outbox = use_state(|| {
        let (tx, rx) = mpsc::unbounded::<ClientMsg>();
        (tx, Rc::new(RefCell::new(Some(rx))))
    });
    let input_ref = use_node_ref();
    let scroll_ref = use_node_ref();
    // "Stick to bottom" unless the user scrolled up to read history.
    let stick = use_mut_ref(|| true);

    // After every render, follow new content to the bottom while sticking.
    {
        let scroll_ref = scroll_ref.clone();
        let stick = stick.clone();
        use_effect(move || {
            if *stick.borrow() {
                if let Some(el) = scroll_ref.cast::<web_sys::Element>() {
                    el.set_scroll_top(el.scroll_height());
                }
            }
            || ()
        });
    }
    // Track whether the user is near the bottom (re-engages auto-scroll).
    let on_scroll = {
        let scroll_ref = scroll_ref.clone();
        let stick = stick.clone();
        Callback::from(move |_: Event| {
            if let Some(el) = scroll_ref.cast::<web_sys::Element>() {
                let from_bottom = el.scroll_height() - el.scroll_top() - el.client_height();
                *stick.borrow_mut() = from_bottom <= 48;
            }
        })
    };

    // One task per Session: connect, relay both ways, and reconnect on a
    // transient drop (resuming the journal from the last seq seen). It ends when
    // the session truly ends (`Ended`) or the component unmounts — unmount drops
    // every outbound sender, so `rx` closes and the loop falls through.
    {
        let model = model.clone();
        let outbox = outbox.clone();
        let token = props.token.clone();
        let id = props.id.clone();
        use_effect_with(props.id.clone(), move |_| {
            if let Some(mut rx) = outbox.1.borrow_mut().take() {
                wasm_bindgen_futures::spawn_local(async move {
                    // First seq we still need; `None` = replay the whole journal.
                    let mut next_seq: Option<u64> = None;
                    let mut attempt: u32 = 0;
                    // Consecutive connections that opened but delivered no message
                    // (e.g. a gone/unreachable session). Bail after a few so we
                    // don't flash "reconnecting…" forever.
                    let mut dead: u32 = 0;
                    'reconnect: loop {
                        if dead >= 3 {
                            model.dispatch(Action::Server(ServerMsg::Ended {
                                reason: "could not connect to this session".into(),
                            }));
                            break;
                        }
                        let ws = match WebSocket::open(&ws_url(&id, &token, next_seq)) {
                            Ok(ws) => ws,
                            Err(_) => {
                                dead += 1;
                                backoff(&mut attempt).await;
                                continue;
                            }
                        };
                        attempt = 0;
                        let (mut write, mut read) = ws.split();
                        let mut terminal = false;
                        let mut got_msg = false;
                        loop {
                            futures::select! {
                                incoming = read.next().fuse() => match incoming {
                                    Some(Ok(WsMessage::Text(txt))) => {
                                        if let Ok(msg) = serde_json::from_str::<ServerMsg>(&txt) {
                                            // Clear "reconnecting" only once a real
                                            // message arrives — a connection that
                                            // opens then dies sends nothing, so we
                                            // never flash Connected for it.
                                            if !got_msg {
                                                model.dispatch(Action::Connected);
                                            }
                                            got_msg = true;
                                            if let ServerMsg::Event { seq, .. } = &msg {
                                                next_seq = Some(seq + 1);
                                            }
                                            if matches!(msg, ServerMsg::Ended { .. }) {
                                                terminal = true;
                                            }
                                            model.dispatch(Action::Server(msg));
                                        }
                                    }
                                    _ => break, // socket closed/errored
                                },
                                cmd = rx.next().fuse() => match cmd {
                                    Some(c) => {
                                        let json = serde_json::to_string(&c).unwrap_or_default();
                                        if write.send(WsMessage::Text(json)).await.is_err() {
                                            break;
                                        }
                                    }
                                    // Outbound channel closed → the component unmounted.
                                    None => break 'reconnect,
                                },
                            }
                        }
                        if terminal {
                            break;
                        }
                        // A real drop (we'd received data) resets the counter; a
                        // connection that never spoke counts toward giving up.
                        dead = if got_msg { 0 } else { dead + 1 };
                        model.dispatch(Action::Disconnected);
                        backoff(&mut attempt).await;
                    }
                });
            }
            // Teardown is implicit: unmounting drops the senders, closing `rx`.
            || ()
        });
    }

    let send = {
        let tx = outbox.0.clone();
        move |cmd: ClientMsg| {
            let _ = tx.unbounded_send(cmd);
        }
    };

    // Submit the input box: echo it locally for instant feedback and send it. The
    // worker journals a `UserMessage` (deduped against this echo) so it also
    // survives a refresh and reaches other clients.
    let on_submit = {
        let model = model.clone();
        let input_ref = input_ref.clone();
        let send = send.clone();
        Callback::from(move |_| {
            if let Some(ta) = input_ref.cast::<HtmlTextAreaElement>() {
                let text = ta.value().trim().to_string();
                if !text.is_empty() {
                    model.dispatch(Action::User(text.clone()));
                    send(ClientMsg::Message(text));
                    ta.set_value("");
                }
            }
        })
    };
    // Enter sends; Shift+Enter inserts a newline.
    let on_keydown = {
        let on_submit = on_submit.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" && !e.shift_key() {
                e.prevent_default();
                on_submit.emit(());
            }
        })
    };

    let interrupt = {
        let send = send.clone();
        Callback::from(move |_| {
            send(ClientMsg::Interrupt {
                kind: InterruptKind::Turn,
            })
        })
    };
    let back = props.on_back.clone();
    let on_back = Callback::from(move |_| back.emit(()));

    html! {
        <div class="page">
            <header class="bar">
                <button class="ghost" onclick={on_back}>{ "‹" }</button>
                <span class="title">{ title(&model) }</span>
                <span class="muted stats">
                    { format!("{} in · {} out", model.tokens_in, model.tokens_out) }
                    if model.cost_usd > 0.0 { { format!(" · ${:.3}", model.cost_usd) } }
                </span>
            </header>

            if let Some(reason) = &model.blocked {
                <div class="banner blocked">{ format!("⏸ blocked: {reason}") }</div>
            }
            if !model.plan.is_empty() {
                <ul class="plan">
                    { for model.plan.iter().map(|(step, st)| html!{
                        <li class={plan_class(st)}>{ plan_mark(st) }{ " " }{ step.clone() }</li>
                    }) }
                </ul>
            }

            <main class="transcript" ref={scroll_ref} onscroll={on_scroll}>
                { for model.blocks.iter().map(render_block) }
                if !model.reasoning.is_empty() {
                    <pre class="reasoning">{ model.reasoning.clone() }</pre>
                }
                if !model.streaming.is_empty() {
                    // Render the in-progress answer as markdown live (incomplete
                    // markdown renders gracefully), so it doesn't snap from raw to
                    // formatted when the turn finishes.
                    <div class="agent streaming">{ markdown(&model.streaming) }</div>
                }
                if model.running {
                    <div class="spinner muted">{ "…working" }</div>
                }
                { conn_banner(&model.conn) }
            </main>

            if let Some(ask) = &model.ask { { render_ask(ask, send.clone()) } }
            if let Some(ap) = &model.approval { { render_approval(ap, send.clone()) } }

            <footer class="composer">
                <textarea ref={input_ref} placeholder="Message the agent…  (Enter to send)"
                    onkeydown={on_keydown} rows="1" />
                <button class="send" onclick={on_submit.reform(|_| ())}>{ "Send" }</button>
                if model.running {
                    <button class="ghost" onclick={interrupt} title="interrupt the current turn">{ "■" }</button>
                }
            </footer>
        </div>
    }
}

fn ws_url(id: &str, token: &str, since_seq: Option<u64>) -> String {
    let loc = web_sys::window().unwrap().location();
    let proto = if loc.protocol().as_deref() == Ok("https:") {
        "wss"
    } else {
        "ws"
    };
    let host = loc.host().unwrap_or_default();
    let id = js_sys::encode_uri_component(id);
    let token = js_sys::encode_uri_component(token);
    let mut url = format!("{proto}://{host}/api/session/{id}/ws?token={token}");
    // On reconnect, resume the journal from where we left off instead of
    // replaying everything (the server passes this through as the worker Hello).
    if let Some(seq) = since_seq {
        url.push_str(&format!("&since_seq={seq}"));
    }
    url
}

/// Exponential backoff between reconnect attempts: 0.5s → 8s.
async fn backoff(attempt: &mut u32) {
    let ms = (500u32 << (*attempt).min(4)).min(8_000);
    *attempt = attempt.saturating_add(1);
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

fn conn_banner(conn: &model::ConnState) -> Html {
    use model::ConnState::*;
    match conn {
        Reconnecting => html! { <div class="banner reconnecting">{ "reconnecting…" }</div> },
        Ended(reason) => {
            html! { <div class="banner ended">{ format!("session ended: {reason}") }</div> }
        }
        Connecting | Live => html! {},
    }
}

fn title(m: &Model) -> String {
    if m.title.is_empty() {
        "session".into()
    } else {
        m.title.clone()
    }
}

fn render_block(b: &Block) -> Html {
    match b {
        Block::User(t) => html! { <div class="msg user">{ t.clone() }</div> },
        Block::Agent(t) => html! { <div class="agent">{ markdown(t) }</div> },
        Block::Tool(t) => html! { <div class="tool">{ "✎ " }{ t.clone() }</div> },
        Block::Notice(t) => html! { <div class="notice muted">{ t.clone() }</div> },
        Block::Final(t) => html! { <div class="final">{ "✓ " }{ markdown(t) }</div> },
        Block::Command { cmd, output, exit } => html! {
            <div class="command">
                <div class="cmd">{ "$ " }{ cmd.clone() }</div>
                if !output.is_empty() { <pre class="output">{ output.clone() }</pre> }
                if exit.is_some_and(|c| c != 0) {
                    <div class="exit error">{ format!("[exit {}]", exit.unwrap()) }</div>
                }
            </div>
        },
        Block::Diff { path, diff } => html! {
            <div class="diff">
                <div class="diff-path muted">{ path.clone() }</div>
                <pre>{ for diff.lines().map(diff_line) }</pre>
            </div>
        },
    }
}

/// Render agent markdown to sanitized HTML. Raw HTML embedded in the model's
/// output is shown as escaped text (never executed), and link hrefs are limited
/// to safe schemes — the agent's output is untrusted, so this must not be an XSS
/// vector into the page that holds the access token.
fn markdown(src: &str) -> Html {
    use pulldown_cmark::{html, Event, Options, Parser, Tag};
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let events = Parser::new_ext(src, opts).map(|ev| match ev {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let dest_url = if is_safe_url(&dest_url) {
                dest_url
            } else {
                "#".into()
            };
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            })
        }
        ev => ev,
    });
    let mut body = String::new();
    html::push_html(&mut body, events);
    Html::from_html_unchecked(format!("<div class=\"md\">{body}</div>").into())
}

/// Allow only obviously-safe link schemes (block `javascript:`, `data:`, etc.).
fn is_safe_url(url: &str) -> bool {
    let u = url.trim_start();
    u.starts_with("http://")
        || u.starts_with("https://")
        || u.starts_with("mailto:")
        || u.starts_with('/')
        || u.starts_with('#')
}

fn diff_line(line: &str) -> Html {
    let cls = match line.as_bytes().first() {
        Some(b'+') => "add",
        Some(b'-') => "del",
        _ if line.starts_with("@@") => "hunk",
        _ => "ctx",
    };
    html! { <span class={classes!("dl", cls)}>{ line.to_string() }{ "\n" }</span> }
}

fn render_ask(ask: &model::Ask, send: impl Fn(ClientMsg) + Clone + 'static) -> Html {
    let opts = ask.options.iter().map(|o| {
        let id = ask.id;
        let o2 = o.clone();
        let s = send.clone();
        let onclick = Callback::from(move |_| {
            s(ClientMsg::AskReply {
                id,
                answer: o2.clone(),
            })
        });
        html! { <button {onclick}>{ o.clone() }</button> }
    });
    html! {
        <div class="modal">
            <div class="modal-card">
                <p class="q">{ ask.question.clone() }</p>
                <div class="opts">{ for opts }</div>
            </div>
        </div>
    }
}

fn render_approval(ap: &model::Approval, send: impl Fn(ClientMsg) + Clone + 'static) -> Html {
    let allow = {
        let id = ap.id;
        let send = send.clone();
        Callback::from(move |_| {
            send(ClientMsg::ApprovalReply {
                id,
                verdict: Verdict::Allow,
                scope: ApprovalScope::Session,
            })
        })
    };
    let deny = {
        let id = ap.id;
        Callback::from(move |_| {
            send(ClientMsg::ApprovalReply {
                id,
                verdict: Verdict::Deny,
                scope: ApprovalScope::Once,
            })
        })
    };
    html! {
        <div class="modal">
            <div class="modal-card">
                <p class="q">{ "Allow network access to" }</p>
                <p class="dest">{ ap.dest.clone() }</p>
                <div class="opts">
                    <button class="allow" onclick={allow}>{ "Allow (session)" }</button>
                    <button class="deny" onclick={deny}>{ "Deny" }</button>
                </div>
            </div>
        </div>
    }
}

fn status_label(s: &SessionStatus) -> &'static str {
    match s {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::Idle => "idle",
        SessionStatus::AwaitingApproval => "approval",
        SessionStatus::AwaitingInput => "waiting",
        SessionStatus::Blocked => "blocked",
        SessionStatus::Completed => "done",
        SessionStatus::Failed => "failed",
        SessionStatus::Stale => "stale",
    }
}

fn plan_class(status: &str) -> Classes {
    match status {
        "done" => classes!("plan-step", "done"),
        "in_progress" => classes!("plan-step", "active"),
        _ => classes!("plan-step"),
    }
}

fn plan_mark(status: &str) -> &'static str {
    match status {
        "done" => "✓",
        "in_progress" => "▸",
        _ => "·",
    }
}

fn main() {
    yew::Renderer::<App>::new().render();
}
