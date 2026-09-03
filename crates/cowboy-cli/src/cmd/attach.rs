//! `cowboy attach` — the thin client. Connects to a session worker's socket,
//! replays the journal, then streams live, reusing the existing ratatui
//! `run_event_loop` via a bridge that translates the wire protocol to/from the
//! in-process `UiEvent`/`AgentCmd` channels.
//!
//! The bridge is the heart of the client: a worker `ServerMsg` becomes a
//! `UiEvent` (with `Ask`/`Approval` reply channels synthesized locally and
//! their answers sent back as `ClientMsg`), and an `AgentCmd` from the UI
//! becomes a `ClientMsg` on the socket.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use cowboy_core::daemonproto::{
    AttachTarget, ClientMsg, DaemonReq, DaemonResp, InterruptKind, ServerMsg, SessionInfo,
    UiEventMsg,
};
use cowboy_core::netproto::encode_line;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::agent::tui::{run_event_loop, AgentCmd, SessionCtx, TurnCancel, UiEvent};

/// Attach to a session: by id (via the daemon) or, for testing, a worker socket
/// path directly.
pub async fn run(target: String) -> Result<()> {
    // Direct socket path (mainly for tests / debugging).
    let p = std::path::PathBuf::from(&target);
    if p.exists() && p.extension().is_some_and(|e| e == "sock") {
        let ctx = SessionCtx {
            root: std::env::current_dir().unwrap_or_default(),
            models: Vec::new(),
            current_model: String::new(),
            ranch_id: None,
            workstream_id: None,
        };
        return attach_socket(&p, "cowboy", Vec::new(), ctx);
    }

    // Otherwise treat it as a session id and ask the daemon where to attach.
    let info = match crate::cmd::daemon::request(DaemonReq::GetSession { id: target.clone() }).await
    {
        Ok(DaemonResp::Session { info }) => info,
        Ok(DaemonResp::Err { message }) => anyhow::bail!(message),
        Ok(other) => anyhow::bail!("unexpected daemon response: {other:?}"),
        Err(e) => anyhow::bail!("cowboyd not reachable: {e}"),
    };
    let resp = crate::cmd::daemon::request(DaemonReq::AttachSession { id: target }).await?;
    let target = match resp {
        DaemonResp::Attach { target } => target,
        DaemonResp::Err { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected daemon response: {other:?}"),
    };
    let ctx = SessionCtx {
        root: info.root.clone(),
        models: Vec::new(),
        current_model: String::new(),
        ranch_id: info.ranch_id.clone(),
        workstream_id: info.workstream_id.clone(),
    };
    let title = title_for(&info);
    match target {
        AttachTarget::Live { worker_sock } => {
            // A worker can exit between GetSession and our connect. Probe the
            // socket first; if it's dead, fall back to a read-only journal
            // replay rather than dropping the user into a broken live view.
            if crate::localsock::connect_blocking(&worker_sock).is_ok() {
                attach_socket(
                    &worker_sock,
                    &title,
                    vec![format!("attached to {}", info.id)],
                    ctx,
                )
            } else {
                match info.journal_path.clone() {
                    Some(j) => replay_journal(&j, &title, &format!("{:?}", info.status), ctx),
                    None => anyhow::bail!("session {} is gone and has no journal", info.id),
                }
            }
        }
        AttachTarget::Replay {
            journal_path,
            status,
        } => replay_journal(&journal_path, &title, &format!("{status:?}"), ctx),
    }
}

/// Render a terminal session read-only by replaying its `events.jsonl` from
/// disk. There is no worker socket: we feed every journaled event into the UI,
/// then `Done` so the loop drops into review-only mode.
pub fn replay_journal(
    journal_path: &std::path::Path,
    title: &str,
    status: &str,
    ctx: SessionCtx,
) -> Result<()> {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    // The agent side is dead; nothing consumes commands, so the receiver is
    // dropped immediately and any input the user types is silently ignored.
    let (task_tx, _task_rx) = std::sync::mpsc::channel::<AgentCmd>();
    let turn_cancel: TurnCancel = std::sync::Arc::new(std::sync::Mutex::new(None));

    let events = read_journal(journal_path);
    let loop_tx = ui_tx.clone();
    let feeder = std::thread::spawn(move || {
        for event in events {
            if ui_tx.send(UiEvent::Wire(event)).is_err() {
                return;
            }
        }
        let _ = ui_tx.send(UiEvent::Done);
    });

    let intro = vec![format!("replay of {status} session (read-only)")];
    run_event_loop(
        title,
        intro,
        None,
        ui_rx,
        loop_tx,
        task_tx,
        turn_cancel,
        ctx,
    )?;
    let _ = feeder.join();
    Ok(())
}

/// Read every journaled [`UiEventMsg`] from an `events.jsonl` (one per line),
/// skipping any unparseable lines.
fn read_journal(path: &std::path::Path) -> Vec<UiEventMsg> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<UiEventMsg>(l.trim()).ok())
        .collect()
}

/// Run the TUI attached to `sock`. The terminal event loop runs on this thread;
/// the bridge runs on its own thread with a tokio runtime.
pub fn attach_socket(
    sock: &std::path::Path,
    title: &str,
    intro: Vec<String>,
    ctx: SessionCtx,
) -> Result<()> {
    attach_socket_ro(sock, title, intro, ctx, false)
}

/// As [`attach_socket`], but `read_only` watches the session without driving it:
/// the client announces itself read-only and never forwards input.
pub fn attach_socket_ro(
    sock: &std::path::Path,
    title: &str,
    intro: Vec<String>,
    ctx: SessionCtx,
    read_only: bool,
) -> Result<()> {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
    let turn_cancel: TurnCancel =
        std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));

    // The event loop keeps a sender too (for client-side async results like the
    // fetched model list); the bridge thread takes its own clone.
    let loop_tx = ui_tx.clone();
    let sock = sock.to_path_buf();
    let bridge_cancel = turn_cancel.clone();
    let handle = std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            let _ = ui_tx.send(UiEvent::Done);
            return;
        };
        rt.block_on(async move {
            match crate::localsock::connect(&sock).await {
                Ok(stream) => {
                    let _ = bridge(stream, ui_tx.clone(), task_rx, bridge_cancel, read_only).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(format!(
                        "attach failed: {e}"
                    ))));
                }
            }
            let _ = ui_tx.send(UiEvent::Done);
        });
    });

    run_event_loop(
        title,
        intro,
        None,
        ui_rx,
        loop_tx,
        task_tx,
        turn_cancel,
        ctx,
    )?;
    let _ = handle.join();
    Ok(())
}

/// Bridge a connected worker `stream` to the UI channels. Returns when the
/// worker ends or the UI hangs up. A `read_only` client never forwards input
/// (no `Message`/`SwitchModel`), so it can watch without driving the session.
pub async fn bridge(
    stream: UnixStream,
    ui_tx: Sender<UiEvent>,
    task_rx: Receiver<AgentCmd>,
    turn_cancel: TurnCancel,
    read_only: bool,
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let (out_tx, mut out_rx) = unbounded_channel::<ClientMsg>();

    // Subscribe from the start.
    out_tx
        .send(ClientMsg::Hello {
            since_seq: None,
            read_only,
        })
        .ok();

    // Single writer: drain ClientMsgs to the socket.
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if w.write_all(encode_line(&msg).as_bytes()).await.is_err() {
                break;
            }
            let _ = w.flush().await;
        }
    });

    // A local detach (the user chose "detach" in the pause menu) leaves the
    // session running, so no `Ended` is coming. The reader would then block
    // forever waiting for the worker to close the socket, hanging the client's
    // `handle.join()`. cmd_pump signals this channel so the bridge tears itself
    // down immediately instead.
    let (detach_tx, mut detach_rx) = tokio::sync::oneshot::channel::<()>();

    // UI commands (blocking std recv) -> ClientMsg. On hangup, end the session.
    // A read-only client drops input but still drains the channel and ends on
    // hangup; it must never send `End` (that would stop a session it's only
    // watching).
    let cmd_out = out_tx.clone();
    let cmd_pump = tokio::task::spawn_blocking(move || {
        while let Ok(cmd) = task_rx.recv() {
            // An explicit detach leaves the session running; tell the worker, then
            // signal the bridge to exit without waiting for an `Ended`.
            if let AgentCmd::Detach = cmd {
                let _ = cmd_out.send(ClientMsg::Detach);
                let _ = detach_tx.send(());
                return;
            }
            if read_only {
                continue;
            }
            // An explicit end: forward it and stop pumping. Returning here rather
            // than falling through to the hangup path means exactly one `End` is
            // sent, whether the client asked for it or simply hung up.
            if let AgentCmd::End = cmd {
                let _ = cmd_out.send(ClientMsg::End);
                return;
            }
            let msg = match cmd {
                AgentCmd::Message(m) => ClientMsg::Message(m),
                AgentCmd::SwitchModel(n) => ClientMsg::SwitchModel(n),
                AgentCmd::PlanMode(b) => ClientMsg::PlanMode(b),
                AgentCmd::Accept { note } => ClientMsg::Accept { note },
                AgentCmd::Detach | AgentCmd::End => unreachable!("handled above"),
            };
            if cmd_out.send(msg).is_err() {
                return;
            }
        }
        let _ = cmd_out.send(if read_only {
            ClientMsg::Detach
        } else {
            ClientMsg::End
        });
    });

    // Interrupt watcher: when the UI fires the turn-cancel token, send an
    // Interrupt and re-arm a fresh token for the next turn. Read-only clients
    // don't interrupt the session they're watching.
    let int_out = out_tx.clone();
    let int_cancel = turn_cancel.clone();
    let interrupts = tokio::spawn(async move {
        loop {
            let token = int_cancel.lock().unwrap().clone();
            let Some(token) = token else { break };
            token.cancelled().await;
            if !read_only
                && int_out
                    .send(ClientMsg::Interrupt {
                        kind: InterruptKind::Turn,
                    })
                    .is_err()
            {
                break;
            }
            *int_cancel.lock().unwrap() = Some(CancellationToken::new());
        }
    });

    // Reader: worker ServerMsg -> UiEvent (+ reply synthesis). Runs as a task so a
    // local detach can tear the bridge down without waiting on it — otherwise it
    // would block on `read_line` until the worker closes the socket, which never
    // happens for a detach (the session stays up).
    let read_out = out_tx.clone();
    let mut reader_task = tokio::spawn(async move {
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(msg) = serde_json::from_str::<ServerMsg>(line.trim()) else {
                continue;
            };
            if !handle_server_msg(msg, &ui_tx, &read_out) {
                break; // Ended
            }
        }
    });

    // Exit when the worker ends/closes the socket, or when the user detaches.
    //
    // `Ok(())` matters: `detach_tx` lives in `cmd_pump`, so it is *dropped* whenever
    // that task returns — including right after it queues an `End`. A bare `_ =`
    // arm therefore fired on the drop, and the teardown below aborted the writer
    // before the `End` had been written to the socket. The user saw "session ended"
    // and a worker that kept running, because the request never left the client.
    // Only an explicit send is a detach; a drop means nothing.
    tokio::select! {
        _ = &mut reader_task => {}
        Ok(()) = &mut detach_rx => {}
    }

    // Stop the producers, then let the writer drain what they queued. Aborting it
    // outright was the other half of the bug above: the last message a client sends
    // is precisely the one that says why it is leaving, so dropping it strands the
    // session. Bounded, because a wedged socket must not hold the client open.
    reader_task.abort();
    interrupts.abort();
    cmd_pump.abort();
    drop(out_tx);
    if tokio::time::timeout(Duration::from_secs(2), &mut writer)
        .await
        .is_err()
    {
        tracing::debug!("outbound queue did not drain before teardown");
        writer.abort();
    }
    Ok(())
}

/// Translate one `ServerMsg` into UI events. Returns false when the session has
/// ended (the caller should stop reading).
fn handle_server_msg(
    msg: ServerMsg,
    ui_tx: &Sender<UiEvent>,
    out_tx: &UnboundedSender<ClientMsg>,
) -> bool {
    match msg {
        ServerMsg::Snapshot { info, .. } => {
            let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Title(title_for(&info))));
        }
        ServerMsg::Event { event, .. } => {
            let _ = ui_tx.send(UiEvent::Wire(event));
        }
        ServerMsg::Ask {
            id,
            question,
            options,
        } => {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel::<String>();
            let _ = ui_tx.send(UiEvent::Ask(question, options, reply_tx));
            let out = out_tx.clone();
            tokio::task::spawn_blocking(move || {
                let answer = reply_rx.recv().unwrap_or_default();
                let _ = out.send(ClientMsg::AskReply { id, answer });
            });
        }
        ServerMsg::Approval { id, dest } => {
            let (vtx, vrx) = tokio::sync::oneshot::channel();
            let _ = ui_tx.send(UiEvent::Approval(dest, vtx));
            let out = out_tx.clone();
            tokio::spawn(async move {
                if let Ok((verdict, scope)) = vrx.await {
                    let _ = out.send(ClientMsg::ApprovalReply { id, verdict, scope });
                }
            });
        }
        ServerMsg::ApprovalResolved { .. } => {
            let _ = ui_tx.send(UiEvent::ApprovalResolved);
        }
        ServerMsg::Status(_) => {}
        ServerMsg::Ended { reason } => {
            if !reason.is_empty() {
                let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(reason)));
            }
            return false;
        }
    }
    true
}

fn title_for(info: &SessionInfo) -> String {
    let cwd = info.root.display();
    match &info.branch {
        Some(b) => format!("{cwd}  ⎇ {b}"),
        None => cwd.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::daemonproto::SessionStatus;
    use tokio::net::UnixListener;

    /// The TUI's "end" drops `task_tx`; the bridge must then send `ClientMsg::End`
    /// to the worker. (Isolates the client half of the "press e, session stays
    /// Running" bug — no worker/daemon involved, just the bridge.)
    #[tokio::test]
    async fn dropping_task_tx_sends_end_to_worker() {
        let dir = assert_fs::TempDir::new().unwrap();
        let sock = dir.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Fake worker: read the ClientMsgs the bridge sends until we see End.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if matches!(
                    serde_json::from_str::<ClientMsg>(line.trim()),
                    Ok(ClientMsg::End)
                ) {
                    return true;
                }
            }
            false
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let turn_cancel: TurnCancel =
            std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel, false));

        // Simulate the pause-menu "end": drop the only task sender.
        drop(task_tx);

        let saw_end = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("timed out waiting for the bridge to send End")
            .unwrap();
        assert!(
            saw_end,
            "dropping task_tx must make the bridge send ClientMsg::End"
        );
        bridge_h.abort();
    }

    /// The reported sequence, which the test above misses: interrupt a turn (`k`),
    /// *then* end (`e`). This is the shape of the "ended the session and the worker
    /// is still running" bug — a plain end works, an end after an interrupt did not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_task_tx_after_an_interrupt_still_sends_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = assert_fs::TempDir::new().unwrap();
        let sock = dir.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Counters rather than a return value: a fake worker that *returns* closes its
        // socket, which makes the bridge tear itself down — a feedback loop that races
        // whatever is still being written. Observing from the outside keeps the
        // connection open for as long as the test needs it.
        let interrupts = std::sync::Arc::new(AtomicUsize::new(0));
        let ends = std::sync::Arc::new(AtomicUsize::new(0));
        let (si, se) = (interrupts.clone(), ends.clone());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ClientMsg>(line.trim()) {
                    Ok(ClientMsg::Interrupt { .. }) => {
                        si.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(ClientMsg::End) => {
                        se.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let turn_cancel: TurnCancel =
            std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel.clone(), false));

        // "k": cancel the in-flight turn. The bridge's watcher turns this into an
        // Interrupt and re-arms a fresh token for the next turn.
        turn_cancel.lock().unwrap().clone().unwrap().cancel();
        await_count(&interrupts, 1, "the interrupt was never forwarded").await;

        // "e": exactly what the pause menu does — send End, drop the sender, then
        // cancel whatever token is current.
        task_tx.send(AgentCmd::End).unwrap();
        drop(task_tx);
        if let Some(tok) = turn_cancel.lock().unwrap().as_ref() {
            tok.cancel();
        }
        await_count(&ends, 1, "ending after an interrupt must still send End").await;

        bridge_h.abort();
        server.abort();
    }

    /// Wait for `counter` to reach `want`, or fail with `why`.
    async fn await_count(counter: &std::sync::atomic::AtomicUsize, want: usize, why: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while counter.load(std::sync::atomic::Ordering::SeqCst) < want {
            assert!(tokio::time::Instant::now() < deadline, "{why}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The explicit end: `e` sends `AgentCmd::End`, and the bridge must forward it as
    /// `ClientMsg::End` exactly once — not twice (the sender is dropped straight
    /// after, which is the backstop path).
    /// On a **current_thread** runtime, deliberately: that is what `attach_socket_ro`
    /// builds, so the writer task and the bridge's own teardown share one thread and
    /// their ordering is decided by the runtime's queue rather than by two CPUs racing.
    /// This is the shape the product actually uses, so it is the shape to assert on.
    #[tokio::test]
    async fn an_explicit_end_is_forwarded_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = assert_fs::TempDir::new().unwrap();
        let sock = dir.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Counted from outside rather than returned: a fake worker that returns closes
        // its socket, and the bridge deliberately no longer tears down until the worker
        // says `Ended` — so waiting for EOF here would wait forever.
        let ends = std::sync::Arc::new(AtomicUsize::new(0));
        let se = ends.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if matches!(
                    serde_json::from_str::<ClientMsg>(line.trim()),
                    Ok(ClientMsg::End)
                ) {
                    se.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let turn_cancel: TurnCancel =
            std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel, false));

        // Exactly what the pause menu's "e" does now.
        task_tx.send(AgentCmd::End).unwrap();
        drop(task_tx);

        await_count(&ends, 1, "the explicit end was never forwarded").await;
        // Then settle, so a second End from the hangup path would be caught.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            ends.load(Ordering::SeqCst),
            1,
            "End must be sent once, not duplicated by the hangup"
        );
        bridge_h.abort();
        server.abort();
    }

    /// A read-only client must not be able to end the session it is watching, however
    /// the end was requested.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_only_client_never_sends_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = assert_fs::TempDir::new().unwrap();
        let sock = dir.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let hellos = std::sync::Arc::new(AtomicUsize::new(0));
        let ends = std::sync::Arc::new(AtomicUsize::new(0));
        let (sh, se) = (hellos.clone(), ends.clone());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ClientMsg>(line.trim()) {
                    Ok(ClientMsg::Hello { .. }) => {
                        sh.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(ClientMsg::End) => {
                        se.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let turn_cancel: TurnCancel =
            std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel, true));

        // The Hello proves the bridge is really talking, so "no End" is a decision and
        // not just a connection that never got going.
        await_count(&hellos, 1, "the read-only client never announced itself").await;
        task_tx.send(AgentCmd::End).unwrap();
        drop(task_tx);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            ends.load(Ordering::SeqCst),
            0,
            "a watcher must not end the session it is only watching"
        );
        bridge_h.abort();
        server.abort();
    }

    fn info() -> SessionInfo {
        SessionInfo {
            id: "t".into(),
            root: "/tmp/app".into(),
            task: None,
            status: SessionStatus::Running,
            pid: None,
            branch: Some("main".into()),
            session_name: None,
            worker_sock: None,
            journal_path: None,
            lease_mode: None,
            started_ms: 0,
            last_heartbeat_ms: 0,
            turn: 0,
            tokens: (0, 0),
            attached_clients: 0,
            diffstat: String::new(),
            running_command: None,
            blocked_reason: None,
            ranch_id: None,
            workstream_id: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_translates_both_directions() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Fake worker: handshake, push events, expect a Message, do an Ask.
        let worker = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.contains("hello"));

            for m in [
                ServerMsg::Snapshot {
                    info: info(),
                    journal_len: 0,
                },
                ServerMsg::Event {
                    seq: 0,
                    event: UiEventMsg::Delta("hi".into()),
                },
            ] {
                w.write_all(encode_line(&m).as_bytes()).await.unwrap();
            }
            w.flush().await.unwrap();

            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert!(
                line.contains("\"message\"") && line.contains("go"),
                "got {line}"
            );

            w.write_all(
                encode_line(&ServerMsg::Ask {
                    id: 7,
                    question: "ok?".into(),
                    options: Vec::new(),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert!(
                line.contains("ask_reply") && line.contains("yes") && line.contains('7'),
                "got {line}"
            );
            w.write_all(
                encode_line(&ServerMsg::Ended {
                    reason: String::new(),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let cancel: TurnCancel = std::sync::Arc::new(std::sync::Mutex::new(None));
        let bridge = tokio::spawn(bridge(stream, ui_tx, task_rx, cancel, false));

        let title = ui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(title, UiEvent::Wire(UiEventMsg::Title(t)) if t.contains("main")));
        let delta = ui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(delta, UiEvent::Wire(UiEventMsg::Delta(t)) if t == "hi"));

        task_tx.send(AgentCmd::Message("go".into())).unwrap();

        let ask = ui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match ask {
            UiEvent::Ask(q, _options, reply) => {
                assert_eq!(q, "ok?");
                reply.send("yes".into()).unwrap();
            }
            other => panic!("expected Ask, got {other:?}"),
        }

        worker.await.unwrap();
        let _ = bridge.await;
    }

    #[test]
    fn read_journal_parses_events_and_skips_garbage() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let journal = tmp.path().join("events.jsonl");
        let lines = [
            serde_json::to_string(&UiEventMsg::ToolUse("read foo".into())).unwrap(),
            "{ not json".to_string(),
            serde_json::to_string(&UiEventMsg::Final("done".into())).unwrap(),
        ];
        std::fs::write(&journal, lines.join("\n")).unwrap();

        let events = read_journal(&journal);
        assert_eq!(events.len(), 2, "garbage line must be skipped");
        assert!(matches!(&events[0], UiEventMsg::ToolUse(s) if s == "read foo"));
        assert!(matches!(&events[1], UiEventMsg::Final(s) if s == "done"));
    }

    #[test]
    fn read_journal_missing_file_is_empty() {
        assert!(read_journal(std::path::Path::new("/nope/missing.jsonl")).is_empty());
    }
}
