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
use web_sys::wasm_bindgen::JsCast;
use web_sys::{Event, HtmlTextAreaElement, KeyboardEvent};
use yew::prelude::*;

use model::{Block, Model};

/// Reducer actions for the session [`Model`].
#[allow(clippy::large_enum_variant)] // one per wire message, dispatched and dropped
pub enum Action {
    Server(ServerMsg),
    /// Optimistic local echo of a message the user just sent.
    User(String),
    /// A fresh WebSocket opened.
    Connected,
    /// Reconnect attempts are currently unavailable, without claiming the session ended.
    Unavailable,
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
            Action::Unavailable => m.set_unavailable(),
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
    // When watching a subagent of the selected session: its subagent id.
    let watching = use_state(|| Option::<String>::None);

    if token.is_empty() {
        return html! {
            <div class="center">
                <h1>{ "cowboy" }</h1>
                <p class="muted">{ "Missing access token. Open the URL printed by " }<code>{ "cowboy web" }</code>{ "." }</p>
            </div>
        };
    }

    match ((*selected).clone(), (*watching).clone()) {
        // Watching a subagent: read-only view of the child's journal.
        (Some(parent), Some(sub)) => {
            let back = {
                let watching = watching.clone();
                Callback::from(move |_| watching.set(None))
            };
            html! {
                <Session id={sub} parent={Some(parent)} token={(*token).clone()} on_back={back} />
            }
        }
        (Some(id), None) => {
            let back = {
                let selected = selected.clone();
                Callback::from(move |_| selected.set(None))
            };
            let on_watch = {
                let watching = watching.clone();
                Callback::from(move |sub: String| watching.set(Some(sub)))
            };
            html! {
                <Session id={id} token={(*token).clone()} on_back={back} on_watch={Some(on_watch)} />
            }
        }
        (None, _) => {
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
    /// When `Some(parent)`, this is a **subagent watch**: stream the child's
    /// journal via `/api/subagent/<parent>/<id>/ws` and render read-only (no
    /// composer/interrupt — subagents are non-interactive).
    #[prop_or_default]
    parent: Option<String>,
    /// Top-level sessions only: open a subagent watch (emits the subagent id).
    #[prop_or_default]
    on_watch: Option<Callback<String>>,
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
        let parent = props.parent.clone();
        use_effect_with((props.id.clone(), props.parent.clone()), move |_| {
            if let Some(mut rx) = outbox.1.borrow_mut().take() {
                wasm_bindgen_futures::spawn_local(async move {
                    // First seq we still need; `None` = replay the whole journal.
                    let mut next_seq: Option<u64> = None;
                    let mut attempt: u32 = 0;
                    // Consecutive connections that opened but delivered no message
                    // (e.g. a gone/unreachable session). After a few, say the session
                    // is unavailable — but keep retrying at the capped backoff: only
                    // a worker's `Ended` means it is really over.
                    let mut dead: u32 = 0;
                    'reconnect: loop {
                        let url = match &parent {
                            Some(p) => subagent_ws_url(p, &id, &token, next_seq),
                            None => ws_url(&id, &token, next_seq),
                        };
                        let ws = match WebSocket::open(&url) {
                            Ok(ws) => ws,
                            Err(_) => {
                                dead += 1;
                                if dead >= 3 {
                                    model.dispatch(Action::Unavailable);
                                }
                                backoff(&mut attempt).await;
                                continue;
                            }
                        };
                        let (mut write, mut read) = ws.split();
                        let mut terminal = false;
                        let mut got_msg = false;
                        loop {
                            futures::select! {
                                incoming = read.next().fuse() => match incoming {
                                    Some(Ok(WsMessage::Text(txt))) => {
                                        let Ok(msg) = serde_json::from_str::<ServerMsg>(&txt) else {
                                            break;
                                        };
                                        // The journal is stateful. Never apply a duplicate,
                                        // regression, or forward gap; reconnect at the last
                                        // contiguous boundary instead.
                                        match &msg {
                                            ServerMsg::Snapshot { journal_len, .. } => {
                                                if next_seq.is_some_and(|seq| seq > *journal_len) {
                                                    break;
                                                }
                                            }
                                            ServerMsg::Event { seq, .. } => {
                                                let expected = next_seq.unwrap_or(0);
                                                if *seq != expected {
                                                    break;
                                                }
                                                next_seq = Some(expected + 1);
                                            }
                                            _ => {}
                                        }
                                        if !got_msg {
                                            // Only a connection that actually speaks
                                            // resets the backoff; `open` succeeding
                                            // says nothing about the session.
                                            attempt = 0;
                                            model.dispatch(Action::Connected);
                                        }
                                        got_msg = true;
                                        if matches!(msg, ServerMsg::Ended { .. }) {
                                            terminal = true;
                                        }
                                        model.dispatch(Action::Server(msg));
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
                        model.dispatch(if dead >= 3 {
                            Action::Unavailable
                        } else {
                            Action::Disconnected
                        });
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
    // Subagent watch is read-only (non-interactive child); a top-level session
    // shows its subagents and offers input.
    let watching = props.parent.is_some();

    html! {
        <div class="page">
            <header class="bar">
                <button class="ghost" onclick={on_back}>{ "‹" }</button>
                <span class="title">
                    if watching { { "👁 " } }
                    { title(&model) }
                </span>
                <span class="muted stats">
                    if let Some(status) = model.status {
                        { format!("{} · ", status_label(&status)) }
                    }
                    { format!("{} in · {} out", model.tokens_in, model.tokens_out) }
                    if let Some((used, budget)) = model.context {
                        // How full the conversation is, which is what you actually want
                        // to know on a small screen — the absolute counts above keep
                        // growing all session and never say how close a compaction is.
                        if budget > 0 {
                            { format!(" · ctx {}%", used.saturating_mul(100) / budget) }
                        }
                    }
                    if model.cost_usd > 0.0 { { format!(" · ${:.3}", model.cost_usd) } }
                </span>
            </header>

            if !watching {
                if let Some(on_watch) = &props.on_watch {
                    if !model.subagents.is_empty() {
                        <div class="subagents">
                            { for model.subagents.iter().map(|s| render_subagent_chip(s, on_watch.clone())) }
                        </div>
                    }
                    if !model.queued.is_empty() {
                        <div class="queued" title={model.queued.join("\n")}>
                            { format!("⏭ {} queued", model.queued.len()) }
                        </div>
                    }
                }
            }

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

            if !watching {
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
            }
        </div>
    }
}

/// A clickable subagent chip; click opens its read-only live watch.
fn render_subagent_chip(s: &model::SubagentStatus, on_watch: Callback<String>) -> Html {
    let id = s.id.clone();
    let onclick = Callback::from(move |_| on_watch.emit(id.clone()));
    // A worker asking for turns is parked, not working, and the number it is waiting on
    // is the actionable part — so it gets its own mark rather than looking "running".
    let (mark, cls) = if s.requested > 0 {
        ("⏸", "asking")
    } else if s.pending {
        ("⋯", "pending")
    } else {
        match s.done {
            None => ("●", "running"),
            Some(true) => ("✓", "done"),
            Some(false) => ("✗", "failed"),
        }
    };
    let detail = if s.requested > 0 {
        format!(" +{}?", s.requested)
    } else if s.granted > 0 && s.done.is_none() && !s.pending {
        format!(" {}/{}", s.used, s.granted)
    } else {
        String::new()
    };
    let tip = if s.requested > 0 {
        format!(
            "{} ({}) is asking for {} more turns — answer it in the session",
            s.label, s.model, s.requested
        )
    } else {
        format!("watch {} ({})", s.label, s.model)
    };
    html! {
        <button class={classes!("subagent-chip", cls)} {onclick} title={tip}>
            { "👁 " }{ s.label.clone() }{ detail }{ " " }<span class="dot">{ mark }</span>
        </button>
    }
}

fn subagent_ws_url(parent: &str, sub: &str, token: &str, since_seq: Option<u64>) -> String {
    let loc = web_sys::window().unwrap().location();
    let proto = if loc.protocol().as_deref() == Ok("https:") {
        "wss"
    } else {
        "ws"
    };
    let host = loc.host().unwrap_or_default();
    let parent = js_sys::encode_uri_component(parent);
    let sub = js_sys::encode_uri_component(sub);
    let token = js_sys::encode_uri_component(token);
    let mut url = format!("{proto}://{host}/api/subagent/{parent}/{sub}/ws?token={token}");
    if let Some(seq) = since_seq {
        url.push_str(&format!("&since_seq={seq}"));
    }
    url
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
        Connecting => html! { <div class="banner reconnecting">{ "connecting…" }</div> },
        Reconnecting => html! { <div class="banner reconnecting">{ "reconnecting…" }</div> },
        Unavailable => {
            html! { <div class="banner reconnecting">{ "session unavailable; retrying…" }</div> }
        }
        Ended(reason) => {
            html! { <div class="banner ended">{ format!("session ended: {reason}") }</div> }
        }
        Live => html! {},
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
    Html::from_html_unchecked(format!("<div class=\"md\">{}</div>", markdown_html(src)).into())
}

/// Render model markdown to an HTML fragment.
///
/// Split from [`markdown`] so the sanitising can be tested without a browser: the yew
/// `Html` wrapper is untestable here, and this is the part with the security properties.
fn markdown_html(src: &str) -> String {
    use pulldown_cmark::{html, Event, Options, Parser, Tag, TagEnd};
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
        // An image becomes a *link*, so nothing is fetched until the user asks.
        //
        // SECURITY: `<img src="…">` is fetched the instant the transcript renders, and
        // the URL is model-controlled. That is an egress beacon which bypasses the
        // sandbox network policy completely, because the *browser* makes the request,
        // not the sandbox — the one thing the boundary is supposed to make impossible.
        // Rendering the alt text as a click-through link keeps the information and
        // removes the automatic fetch; a click is the user's decision, not the agent's.
        //
        // Not merely scheme-checked like a link: a perfectly valid
        // `https://attacker.example/pixel.gif` is exactly the beacon. The scheme check
        // still applies on top, so an unsafe URL degrades to a dead `#`.
        Event::Start(Tag::Image {
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
        Event::End(TagEnd::Image) => Event::End(TagEnd::Link),
        ev => ev,
    });
    let mut body = String::new();
    html::push_html(&mut body, events);
    body
}

/// Allow only obviously-safe link schemes (block `javascript:`, `data:`, etc.).
/// Allow only obviously-safe link schemes (block `javascript:`, `data:`, etc.),
/// and reject any control character.
///
/// The URL is model-controlled and flows into raw HTML (`push_html` →
/// `from_html_unchecked`), so this must be at least as strict as the TUI's
/// `markdown::is_safe_url`: an embedded control character (newline, etc.) could
/// break out of the `href` attribute context. Relative (`/`) and anchor (`#`)
/// targets are allowed here — unlike the TUI — because they are legitimate in a
/// browser `href` and cannot carry a scheme.
fn is_safe_url(url: &str) -> bool {
    if url.chars().any(|c| c.is_control()) {
        return false;
    }
    let u = url.trim_start();
    let scheme_ok = |p: &str| u.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
    scheme_ok("http://")
        || scheme_ok("https://")
        || scheme_ok("mailto:")
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
    let input_id = format!("ask-reply-{}", ask.id);
    let submit = {
        let id = ask.id;
        let input_id = input_id.clone();
        let send = send.clone();
        Callback::from(move |_| {
            let Some(input) = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.get_element_by_id(&input_id))
                .and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok())
            else {
                return;
            };
            let answer = input.value().trim().to_string();
            if !answer.is_empty() {
                send(ClientMsg::AskReply { id, answer });
            }
        })
    };
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
                <textarea id={input_id} placeholder="Type another answer…" rows="2" />
                <div class="opts"><button onclick={submit}>{ "Reply" }</button></div>
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
    // Detail rows when the worker sent them; the flat destination is the fallback,
    // and stays as the heading either way so the answer to "to what?" is never
    // further away than the buttons.
    let rows: Html = ap
        .rows
        .iter()
        .map(|(label, value)| {
            html! {
                <div class="approval-row">
                    <span class="approval-label">{ label.clone() }</span>
                    <span class="approval-value">{ value.clone() }</span>
                </div>
            }
        })
        .collect();
    let note = ap.note.clone().map(|n| html! { <p class="note">{ n }</p> });
    html! {
        <div class="modal">
            <div class="modal-card">
                <p class="q">{ ap.title.clone() }</p>
                <p class="dest">{ ap.dest.clone() }</p>
                { rows }
                { note.unwrap_or_default() }
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

#[cfg(test)]
mod url_tests {
    use super::is_safe_url;

    #[test]
    fn is_safe_url_matches_the_tui_hardening() {
        // Allowed schemes (case-insensitive) + relative/anchor targets.
        assert!(is_safe_url("https://example.com/a?b=c#d"));
        assert!(is_safe_url("HTTP://EXAMPLE.COM"));
        assert!(is_safe_url("mailto:someone@example.com"));
        assert!(is_safe_url("/relative/path"));
        assert!(is_safe_url("#anchor"));

        // Dangerous schemes are blocked.
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("JavaScript:alert(1)"));
        assert!(!is_safe_url("data:text/html,<script>"));
        assert!(!is_safe_url("vbscript:msgbox"));

        // Control characters are rejected — the parity gap with the TUI. An href
        // built into raw HTML must not carry a newline that could break the context.
        assert!(!is_safe_url("https://example.com/\n"));
        assert!(!is_safe_url("/path\twith\ttabs"));
        assert!(!is_safe_url("https://exa\u{0}mple.com"));
    }
}

#[cfg(test)]
mod markdown_tests {
    use super::markdown_html;

    /// Model output must not be able to make the browser fetch anything on its own.
    ///
    /// `<img src>` is fetched the moment the transcript renders, so an image in an agent
    /// answer was an egress beacon that bypassed the sandbox network policy entirely —
    /// the browser makes the request, not the sandbox. The page also carries the bearer
    /// token in its URL, so the fetch is a referer risk on top of the egress one.
    #[test]
    fn an_image_never_becomes_an_img_tag() {
        let out = markdown_html("![pixel](https://attacker.example/pixel.gif)");
        assert!(
            !out.contains("<img"),
            "an image must not render as a fetching tag: {out}"
        );
        // The information is kept as a click-through, so nothing is silently dropped.
        assert!(
            out.contains("<a href=\"https://attacker.example/pixel.gif\""),
            "{out}"
        );
        assert!(out.contains("pixel"), "the alt text should survive: {out}");
    }

    #[test]
    fn an_unsafe_image_url_is_defused_like_an_unsafe_link() {
        for src in [
            "![x](javascript:alert(1))",
            "![x](data:text/html,<script>alert(1)</script>)",
        ] {
            let out = markdown_html(src);
            assert!(!out.contains("<img"), "{src} -> {out}");
            assert!(!out.contains("javascript:"), "{src} -> {out}");
            assert!(!out.contains("data:text/html"), "{src} -> {out}");
            assert!(out.contains("href=\"#\""), "{src} -> {out}");
        }
    }

    #[test]
    fn a_reference_style_image_is_also_covered() {
        // A different parser path to the same tag — worth pinning, because the fix keys
        // on the event rather than on the syntax.
        let out = markdown_html("![alt][ref]\n\n[ref]: https://attacker.example/p.png");
        assert!(!out.contains("<img"), "{out}");
        assert!(out.contains("https://attacker.example/p.png"), "{out}");
    }

    #[test]
    fn ordinary_markdown_still_renders() {
        // The sanitising must not have broken the actual feature.
        let out = markdown_html("**bold** and `code` and [a link](https://example.com)\n\n- item");
        assert!(out.contains("<strong>bold</strong>"), "{out}");
        assert!(out.contains("<code>code</code>"), "{out}");
        assert!(
            out.contains("<a href=\"https://example.com\">a link</a>"),
            "{out}"
        );
        assert!(out.contains("<li>item</li>"), "{out}");
    }

    #[test]
    fn inline_html_is_still_neutralised_as_text() {
        // Pre-existing behaviour, re-pinned next to the new image handling since both
        // are the same defence.
        let out = markdown_html("<script>alert(1)</script>");
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
    }
}
