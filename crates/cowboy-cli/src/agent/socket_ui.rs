//! `SocketUi` — the headless worker's `AgentUi`. Every display event is
//! appended to an `events.jsonl` journal (one [`UiEventMsg`] per line; the line
//! number is its `seq`) and broadcast to attached clients over a per-session
//! Unix socket. On connect a client replays `[since_seq..journal_len)` from the
//! file then switches to the live broadcast — under a single lock, so there are
//! no gaps or duplicates.
//!
//! Network approvals and `ask_user` are routed to attached clients as
//! `ServerMsg::Approval`/`Ask`; the first reply wins. Both fail closed when no
//! client is attached (approvals `Deny`/`Once`, `ask_user` returns "").

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cowboy_core::daemonproto::{ClientMsg, ServerMsg, SessionInfo, UiEventMsg};
use cowboy_core::netproto::{encode_line, ApprovalScope, Verdict};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex as AsyncMutex};

use super::ui::AgentUi;

/// Hard cap on how long a parked gateway connection waits for a verdict before
/// failing closed. A gateway connection is blocked awaiting this answer, so it
/// must never hang indefinitely.
const APPROVAL_TIMEOUT: Duration =
    Duration::from_secs(cowboy_core::netproto::APPROVAL_TIMEOUT_SECS);

/// How long `ask_user` waits for a human answer before giving up (returns "").
const ASK_TIMEOUT: Duration = Duration::from_secs(600);

/// Live broadcast item: a server message (journaled `Event`s plus control
/// messages like `Ask`/`Approval`/`Ended`).
type Live = ServerMsg;

struct Journal {
    file: std::fs::File,
    path: PathBuf,
    len: u64,
}

struct Inner {
    /// Guards journal append + length so a new subscriber's replay/live handoff
    /// is atomic.
    journal: std::sync::Mutex<Journal>,
    live: broadcast::Sender<Live>,
    /// Latest snapshot metadata, sent to each new client.
    info: std::sync::Mutex<SessionInfo>,
    /// Count of currently attached clients.
    attached: AtomicU32,
    /// Monotonic id for outstanding `Ask`/`Approval` prompts (disjoint spaces
    /// are unnecessary — the maps are keyed separately).
    next_req_id: AtomicU64,
    /// Outstanding approvals awaiting a client verdict, keyed by request id.
    pending_approvals: std::sync::Mutex<HashMap<u64, oneshot::Sender<(Verdict, ApprovalScope)>>>,
    /// Outstanding `ask_user` questions awaiting a client answer.
    pending_asks: std::sync::Mutex<HashMap<u64, std::sync::mpsc::Sender<String>>>,
    /// Live progress, mirrored from the event stream so the daemon registry
    /// (`cowboy sessions`) can show real numbers without parsing the journal.
    stats: std::sync::Mutex<SessionStats>,
}

/// Snapshot of a session's live progress for the daemon registry.
#[derive(Clone, Default)]
pub struct SessionStats {
    pub turn: u64,
    pub tokens: (u64, u64),
    pub diffstat: String,
    pub running_command: Option<String>,
    /// Set while the session has declared itself blocked.
    pub blocked_reason: Option<String>,
}

/// Handle to the worker's UI: cloneable, shared between the agent loop (which
/// holds `&mut SocketUi`) and the socket server task.
#[derive(Clone)]
pub struct SocketUi {
    inner: Arc<Inner>,
}

impl SocketUi {
    /// Bind the per-session socket and open the journal. Returns the handle plus
    /// a receiver of client messages (input) the worker should drain.
    pub async fn bind(
        socket_path: &Path,
        journal_path: &Path,
        info: SessionInfo,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ClientMsg>)> {
        if let Some(parent) = socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding session socket {}", socket_path.display()))?;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal_path)
            .with_context(|| format!("opening journal {}", journal_path.display()))?;
        let len = std::fs::read_to_string(journal_path)
            .map(|s| s.lines().count() as u64)
            .unwrap_or(0);

        let (live, _) = broadcast::channel(4096);
        let inner = Arc::new(Inner {
            journal: std::sync::Mutex::new(Journal {
                file,
                path: journal_path.to_path_buf(),
                len,
            }),
            live,
            info: std::sync::Mutex::new(info),
            attached: AtomicU32::new(0),
            next_req_id: AtomicU64::new(0),
            pending_approvals: std::sync::Mutex::new(HashMap::new()),
            pending_asks: std::sync::Mutex::new(HashMap::new()),
            stats: std::sync::Mutex::new(SessionStats::default()),
        });

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let server_inner = inner.clone();
        tokio::spawn(async move {
            accept_loop(listener, server_inner, cmd_tx).await;
        });

        Ok((Self { inner }, cmd_rx))
    }

    /// Number of currently attached clients.
    pub fn attached(&self) -> u32 {
        self.inner
            .attached
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Update the snapshot metadata new clients receive.
    pub fn set_info(&self, info: SessionInfo) {
        *self
            .inner
            .info
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = info;
    }

    /// Journal + broadcast a display event (worker-originated events like
    /// `DiffStat`/`Title`/`Processes`/`TurnDone` use this directly).
    pub fn emit(&self, event: UiEventMsg) {
        self.track(&event);
        let mut j = self
            .inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = j.len;
        let line = serde_json::to_string(&event).unwrap_or_default();
        let _ = writeln!(j.file, "{line}");
        let _ = j.file.flush();
        j.len += 1;
        drop(j);
        let _ = self.inner.live.send(ServerMsg::Event { seq, event });
    }

    /// Mirror progress-bearing events into `stats` for the daemon registry.
    fn track(&self, event: &UiEventMsg) {
        let mut s = self
            .inner
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match event {
            UiEventMsg::Tokens { input, output } => s.tokens = (*input, *output),
            UiEventMsg::Blocked(reason) => s.blocked_reason = reason.clone(),
            UiEventMsg::DiffStat(d) => s.diffstat = d.clone(),
            UiEventMsg::TurnDone => {
                s.turn += 1;
                s.running_command = None;
            }
            UiEventMsg::CommandStart(c) => s.running_command = Some(c.clone()),
            UiEventMsg::CommandEnd { .. } => s.running_command = None,
            _ => {}
        }
    }

    /// A snapshot of live progress for the daemon registry.
    pub fn stats(&self) -> SessionStats {
        self.inner
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Broadcast a terminal `Ended` to attached clients (worker shutting down).
    pub fn end(&self, reason: &str) {
        let _ = self.inner.live.send(ServerMsg::Ended {
            reason: reason.to_string(),
        });
    }

    /// Wait (up to `timeout`) for at least one client to attach. Returns whether
    /// one is attached. Used to avoid auto-denying a startup approval prompt
    /// before the interactive client has had a chance to connect.
    pub async fn wait_for_client(&self, timeout: Duration) -> bool {
        let deadline = timeout;
        let step = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        while self.attached() == 0 && waited < deadline {
            tokio::time::sleep(step).await;
            waited += step;
        }
        self.attached() > 0
    }

    /// Ask attached clients to approve a network destination. Fails closed: with
    /// zero attached clients (or on timeout) the verdict is `Deny`/`Once` so a
    /// parked gateway connection never hangs. With clients, the first
    /// [`ClientMsg::ApprovalReply`] wins; a follow-up `ApprovalResolved` tells
    /// the others to dismiss their modal.
    pub async fn request_approval(&self, dest: String) -> (Verdict, ApprovalScope) {
        if self.attached() == 0 {
            return (Verdict::Deny, ApprovalScope::Once);
        }
        let id = self.inner.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        let _ = self.inner.live.send(ServerMsg::Approval {
            id,
            dest: dest.clone(),
        });

        let verdict = match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            // Sender dropped (no reply) or timed out -> fail closed.
            _ => (Verdict::Deny, ApprovalScope::Once),
        };
        self.inner
            .pending_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        // Tell any other clients still showing this approval to dismiss it.
        let _ = self.inner.live.send(ServerMsg::ApprovalResolved { id });
        verdict
    }
}

impl AgentUi for SocketUi {
    fn model_delta(&mut self, text: &str) {
        self.emit(UiEventMsg::Delta(text.to_string()));
    }
    fn model_reasoning(&mut self, text: &str) {
        self.emit(UiEventMsg::Reasoning(text.to_string()));
    }
    fn model_done(&mut self) {
        self.emit(UiEventMsg::ModelDone);
    }
    fn command_start(&mut self, command: &str) {
        self.emit(UiEventMsg::CommandStart(command.to_string()));
    }
    fn command_output(&mut self, chunk: &str) {
        self.emit(UiEventMsg::CommandOutput(chunk.to_string()));
    }
    fn command_end(&mut self, exit_code: i32, output: &str) {
        self.emit(UiEventMsg::CommandEnd {
            code: exit_code,
            output: output.to_string(),
        });
    }
    fn tool_use(&mut self, summary: &str) {
        self.emit(UiEventMsg::ToolUse(summary.to_string()));
    }
    fn file_diff(&mut self, path: &str, diff: &str) {
        self.emit(UiEventMsg::FileDiff {
            path: path.to_string(),
            diff: diff.to_string(),
        });
    }
    fn tokens(&mut self, input: u64, output: u64) {
        self.emit(UiEventMsg::Tokens { input, output });
    }
    fn cost(&mut self, usd: f64) {
        self.emit(UiEventMsg::Cost(usd));
    }
    fn blocked(&mut self, reason: Option<&str>) {
        self.emit(UiEventMsg::Blocked(reason.map(str::to_string)));
    }
    fn plan(&mut self, steps: &[(String, String)]) {
        self.emit(UiEventMsg::Plan(steps.to_vec()));
    }
    fn subagent_started(&mut self, label: &str, model: &str) {
        self.emit(UiEventMsg::SubagentStarted {
            label: label.to_string(),
            model: model.to_string(),
        });
    }
    fn subagent_done(&mut self, label: &str, ok: bool) {
        self.emit(UiEventMsg::SubagentDone {
            label: label.to_string(),
            ok,
        });
    }
    fn final_message(&mut self, message: &str) {
        self.emit(UiEventMsg::Final(message.to_string()));
    }
    fn notice(&mut self, msg: &str) {
        self.emit(UiEventMsg::Notice(msg.to_string()));
    }
    fn ask_user(&mut self, question: &str, options: &[String]) -> String {
        // No attached client can answer -> empty (matches the non-interactive
        // / subagent contract).
        if self.attached() == 0 {
            return String::new();
        }
        let id = self.inner.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.inner
            .pending_asks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        let _ = self.inner.live.send(ServerMsg::Ask {
            id,
            question: question.to_string(),
            options: options.to_vec(),
        });
        // The agent loop blocks here for the answer; the reply arrives on a
        // socket-server task (separate runtime thread), so this does not deadlock.
        // First reply wins. Bail to "" early if every client detaches mid-ask (no
        // one left to answer — e.g. a detached ranch workstream), and cap the total
        // wait at ASK_TIMEOUT.
        let deadline = std::time::Instant::now() + ASK_TIMEOUT;
        let answer = loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(a) => break a,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break String::new(),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if self.attached() == 0 || std::time::Instant::now() >= deadline {
                        break String::new();
                    }
                }
            }
        };
        self.inner
            .pending_asks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        answer
    }
}

async fn accept_loop(
    listener: UnixListener,
    inner: Arc<Inner>,
    cmd_tx: mpsc::UnboundedSender<ClientMsg>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let inner = inner.clone();
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            inner
                .attached
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = serve_client(stream, &inner, cmd_tx).await;
            inner
                .attached
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
}

async fn serve_client(
    stream: UnixStream,
    inner: &Inner,
    cmd_tx: mpsc::UnboundedSender<ClientMsg>,
) -> Result<()> {
    let (r, w) = stream.into_split();
    let writer = Arc::new(AsyncMutex::new(w));
    let mut reader = BufReader::new(r);

    // First line: optional Hello{since_seq}.
    let mut first = String::new();
    let since = if reader.read_line(&mut first).await? == 0 {
        return Ok(());
    } else {
        match serde_json::from_str::<ClientMsg>(first.trim()) {
            Ok(ClientMsg::Hello { since_seq, .. }) => since_seq.unwrap_or(0),
            // A non-Hello first line is treated as input + a full replay.
            Ok(other) => {
                let _ = cmd_tx.send(other);
                0
            }
            Err(_) => 0,
        }
    };

    // Atomically: subscribe to live, snapshot length, read the journal slice.
    let (mut rx, journal_len, replay) = {
        let j = inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rx = inner.live.subscribe();
        let len = j.len;
        let replay = read_journal_slice(&j.path, since, len);
        (rx, len, replay)
    };

    let info = inner
        .info
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    send(&writer, &ServerMsg::Snapshot { info, journal_len }).await?;
    for (seq, event) in replay {
        send(&writer, &ServerMsg::Event { seq, event }).await?;
    }

    // Pump live events to this client.
    let live_writer = writer.clone();
    let live = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let ended = matches!(msg, ServerMsg::Ended { .. });
                    if send(&live_writer, &msg).await.is_err() || ended {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Read client input until disconnect.
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if let Ok(msg) = serde_json::from_str::<ClientMsg>(line.trim()) {
                    match msg {
                        ClientMsg::Detach => break,
                        // Approval/ask replies resolve a pending prompt here
                        // (first reply wins); they never reach the agent loop.
                        ClientMsg::ApprovalReply { id, verdict, scope } => {
                            if let Some(tx) = inner
                                .pending_approvals
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(&id)
                            {
                                let _ = tx.send((verdict, scope));
                            }
                        }
                        ClientMsg::AskReply { id, answer } => {
                            if let Some(tx) = inner
                                .pending_asks
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(&id)
                            {
                                let _ = tx.send(answer);
                            }
                        }
                        other => {
                            let _ = cmd_tx.send(other);
                        }
                    }
                }
            }
        }
    }
    live.abort();
    Ok(())
}

/// Read journaled events `[since..len)` (0-based seq = line number).
fn read_journal_slice(path: &Path, since: u64, len: u64) -> Vec<(u64, UiEventMsg)> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let seq = i as u64;
            if seq < since || seq >= len {
                return None;
            }
            let line = l.ok()?;
            let event: UiEventMsg = serde_json::from_str(&line).ok()?;
            Some((seq, event))
        })
        .collect()
}

async fn send(
    writer: &AsyncMutex<tokio::net::unix::OwnedWriteHalf>,
    msg: &ServerMsg,
) -> Result<()> {
    let mut w = writer.lock().await;
    w.write_all(encode_line(msg).as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::daemonproto::SessionStatus;

    fn info() -> SessionInfo {
        SessionInfo {
            id: "t".into(),
            root: "/tmp/x".into(),
            task: None,
            status: SessionStatus::Running,
            pid: None,
            branch: None,
            container_name: None,
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

    async fn read_msg(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> ServerMsg {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    #[tokio::test]
    async fn replays_journal_then_streams_live() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");

        let (mut ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();

        // One event before any client connects -> must be replayed.
        ui.command_start("cargo test");

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        w.write_all(
            encode_line(&ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            })
            .as_bytes(),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        // Snapshot first, with journal_len = 1.
        match read_msg(&mut reader).await {
            ServerMsg::Snapshot { journal_len, .. } => assert_eq!(journal_len, 1),
            other => panic!("expected Snapshot, got {other:?}"),
        }
        // Replayed event (seq 0).
        match read_msg(&mut reader).await {
            ServerMsg::Event {
                seq: 0,
                event: UiEventMsg::CommandStart(c),
            } => {
                assert_eq!(c, "cargo test")
            }
            other => panic!("expected replayed CommandStart, got {other:?}"),
        }

        // A live event after connect (seq 1).
        ui.command_end(0, "");
        match read_msg(&mut reader).await {
            ServerMsg::Event {
                seq: 1,
                event: UiEventMsg::CommandEnd { code, .. },
            } => {
                assert_eq!(code, 0)
            }
            other => panic!("expected live CommandEnd, got {other:?}"),
        }

        // The journal on disk holds both events, one per line.
        let lines: Vec<String> = std::fs::read_to_string(&journal)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("command_start"));
    }

    /// Two clients that connect at different points still observe the same
    /// ordered event stream from the moment each is live. A client joining
    /// mid-stream replays the full journal, then both see subsequent live
    /// events in identical order.
    #[tokio::test]
    async fn two_clients_see_identical_order() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");

        let (mut ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();

        // Client A connects first.
        let stream_a = UnixStream::connect(&sock).await.unwrap();
        let (ra, mut wa) = stream_a.into_split();
        let mut reader_a = BufReader::new(ra);
        wa.write_all(
            encode_line(&ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            })
            .as_bytes(),
        )
        .await
        .unwrap();
        wa.flush().await.unwrap();
        // A's snapshot (empty journal).
        match read_msg(&mut reader_a).await {
            ServerMsg::Snapshot { journal_len, .. } => assert_eq!(journal_len, 0),
            other => panic!("expected Snapshot, got {other:?}"),
        }

        // An event lands while only A is attached; give A a moment to drain it
        // so the broadcast ordering across clients is unambiguous.
        ui.tool_use("step one");
        match read_msg(&mut reader_a).await {
            ServerMsg::Event {
                seq: 0,
                event: UiEventMsg::ToolUse(s),
            } => assert_eq!(s, "step one"),
            other => panic!("A expected ToolUse, got {other:?}"),
        }

        // Client B connects mid-stream: it replays the journal (seq 0) first.
        let stream_b = UnixStream::connect(&sock).await.unwrap();
        let (rb, mut wb) = stream_b.into_split();
        let mut reader_b = BufReader::new(rb);
        wb.write_all(
            encode_line(&ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            })
            .as_bytes(),
        )
        .await
        .unwrap();
        wb.flush().await.unwrap();
        match read_msg(&mut reader_b).await {
            ServerMsg::Snapshot { journal_len, .. } => assert_eq!(journal_len, 1),
            other => panic!("B expected Snapshot, got {other:?}"),
        }
        match read_msg(&mut reader_b).await {
            ServerMsg::Event {
                seq: 0,
                event: UiEventMsg::ToolUse(s),
            } => assert_eq!(s, "step one"),
            other => panic!("B expected replayed ToolUse, got {other:?}"),
        }

        // Subsequent live events reach both clients in the same order.
        ui.tool_use("step two");
        ui.tool_use("step three");
        for expected in ["step two", "step three"] {
            for reader in [&mut reader_a, &mut reader_b] {
                match read_msg(reader).await {
                    ServerMsg::Event {
                        event: UiEventMsg::ToolUse(s),
                        ..
                    } => assert_eq!(s, expected),
                    other => panic!("expected live ToolUse {expected}, got {other:?}"),
                }
            }
        }
    }

    /// Connect a client, complete the handshake (Hello -> Snapshot), and return
    /// the split halves. After this returns `attached() >= 1` is guaranteed.
    async fn attach_client(
        sock: &Path,
    ) -> (
        BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        let stream = UnixStream::connect(sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        w.write_all(
            encode_line(&ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            })
            .as_bytes(),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();
        assert!(matches!(
            read_msg(&mut reader).await,
            ServerMsg::Snapshot { .. }
        ));
        (reader, w)
    }

    async fn send_client(w: &mut tokio::net::unix::OwnedWriteHalf, msg: &ClientMsg) {
        w.write_all(encode_line(msg).as_bytes()).await.unwrap();
        w.flush().await.unwrap();
    }

    #[tokio::test]
    async fn approval_denies_with_zero_clients() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let (ui, _cmd_rx) = SocketUi::bind(
            &tmp.path().join("s.sock"),
            &tmp.path().join("events.jsonl"),
            info(),
        )
        .await
        .unwrap();
        // No client attached -> fail closed immediately.
        assert_eq!(
            ui.request_approval("example.com:443".into()).await,
            (Verdict::Deny, ApprovalScope::Once)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approval_first_reply_wins_then_resolves() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &tmp.path().join("events.jsonl"), info())
            .await
            .unwrap();

        let (mut reader, mut w) = attach_client(&sock).await;

        // Ask for approval in the background; the client answers Allow/Session.
        let ask_ui = ui.clone();
        let verdict = tokio::spawn(async move { ask_ui.request_approval("h:443".into()).await });

        let id = match read_msg(&mut reader).await {
            ServerMsg::Approval { id, dest } => {
                assert_eq!(dest, "h:443");
                id
            }
            other => panic!("expected Approval, got {other:?}"),
        };
        send_client(
            &mut w,
            &ClientMsg::ApprovalReply {
                id,
                verdict: Verdict::Allow,
                scope: ApprovalScope::Session,
            },
        )
        .await;

        assert_eq!(
            verdict.await.unwrap(),
            (Verdict::Allow, ApprovalScope::Session)
        );
        // Other clients are told to dismiss the now-decided modal.
        match read_msg(&mut reader).await {
            ServerMsg::ApprovalResolved { id: rid } => assert_eq!(rid, id),
            other => panic!("expected ApprovalResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_user_empty_with_zero_clients() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let (mut ui, _cmd_rx) = SocketUi::bind(
            &tmp.path().join("s.sock"),
            &tmp.path().join("events.jsonl"),
            info(),
        )
        .await
        .unwrap();
        assert_eq!(ui.ask_user("proceed?", &[]), "");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_user_routes_and_first_reply_wins() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &tmp.path().join("events.jsonl"), info())
            .await
            .unwrap();

        let (mut reader, mut w) = attach_client(&sock).await;

        // ask_user blocks, so run it on a blocking thread.
        let mut ask_ui = ui.clone();
        let answer = tokio::task::spawn_blocking(move || ask_ui.ask_user("continue?", &[]));

        let id = match read_msg(&mut reader).await {
            ServerMsg::Ask { id, question, .. } => {
                assert_eq!(question, "continue?");
                id
            }
            other => panic!("expected Ask, got {other:?}"),
        };
        send_client(
            &mut w,
            &ClientMsg::AskReply {
                id,
                answer: "yes".into(),
            },
        )
        .await;
        assert_eq!(answer.await.unwrap(), "yes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_user_returns_empty_when_client_detaches_midask() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &tmp.path().join("events.jsonl"), info())
            .await
            .unwrap();

        let (mut reader, w) = attach_client(&sock).await;
        let mut ask_ui = ui.clone();
        let answer = tokio::task::spawn_blocking(move || ask_ui.ask_user("continue?", &[]));

        // Confirm the ask was routed (so the client is attached and waiting)…
        match read_msg(&mut reader).await {
            ServerMsg::Ask { .. } => {}
            other => panic!("expected Ask, got {other:?}"),
        }
        // …then detach. With no one left to answer, ask_user must give up promptly
        // (poll interval), not block for ASK_TIMEOUT.
        drop(reader);
        drop(w);
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), answer)
            .await
            .expect("ask_user must return promptly after the last client detaches")
            .unwrap();
        assert_eq!(got, "");
    }

    /// `Hello{since_seq: Some(n)}` resumes: the snapshot reports the true
    /// journal length, but only events at seq >= n are replayed.
    #[tokio::test]
    async fn since_seq_resumes_from_offset() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");

        let (mut ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();

        // Three journaled events: seq 0, 1, 2.
        ui.tool_use("zero");
        ui.tool_use("one");
        ui.tool_use("two");

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        w.write_all(
            encode_line(&ClientMsg::Hello {
                since_seq: Some(2),
                read_only: false,
            })
            .as_bytes(),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        // Snapshot reports the full length (3) even though we resume from 2.
        match read_msg(&mut reader).await {
            ServerMsg::Snapshot { journal_len, .. } => assert_eq!(journal_len, 3),
            other => panic!("expected Snapshot, got {other:?}"),
        }
        // Only seq 2 is replayed.
        match read_msg(&mut reader).await {
            ServerMsg::Event {
                seq: 2,
                event: UiEventMsg::ToolUse(s),
            } => assert_eq!(s, "two"),
            other => panic!("expected only seq-2 replay, got {other:?}"),
        }

        // The next thing the client sees is the new live event (seq 3), proving
        // nothing between [0,2) leaked through.
        ui.tool_use("three");
        match read_msg(&mut reader).await {
            ServerMsg::Event {
                seq: 3,
                event: UiEventMsg::ToolUse(s),
            } => assert_eq!(s, "three"),
            other => panic!("expected live seq-3, got {other:?}"),
        }
    }
}
