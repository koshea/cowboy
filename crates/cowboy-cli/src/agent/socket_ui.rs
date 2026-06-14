//! `SocketUi` — the headless worker's `AgentUi`. Every display event is
//! appended to an `events.jsonl` journal (one [`UiEventMsg`] per line; the line
//! number is its `seq`) and broadcast to attached clients over a per-session
//! Unix socket. On connect a client replays `[since_seq..journal_len)` from the
//! file then switches to the live broadcast — under a single lock, so there are
//! no gaps or duplicates.
//!
//! Network-approval / `ask_user` routing over the socket arrives in a later
//! milestone; for now `ask_user` returns empty (no attached approver).

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use cowboy_core::daemonproto::{ClientMsg, ServerMsg, SessionInfo, UiEventMsg};
use cowboy_core::netproto::encode_line;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex};

use super::ui::AgentUi;

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
    attached: std::sync::atomic::AtomicU32,
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
            attached: std::sync::atomic::AtomicU32::new(0),
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
        *self.inner.info.lock().unwrap() = info;
    }

    /// Journal + broadcast a display event (worker-originated events like
    /// `DiffStat`/`Title`/`Processes`/`TurnDone` use this directly).
    pub fn emit(&self, event: UiEventMsg) {
        let mut j = self.inner.journal.lock().unwrap();
        let seq = j.len;
        let line = serde_json::to_string(&event).unwrap_or_default();
        let _ = writeln!(j.file, "{line}");
        let _ = j.file.flush();
        j.len += 1;
        drop(j);
        let _ = self.inner.live.send(ServerMsg::Event { seq, event });
    }

    /// Broadcast a terminal `Ended` to attached clients (worker shutting down).
    pub fn end(&self, reason: &str) {
        let _ = self.inner.live.send(ServerMsg::Ended {
            reason: reason.to_string(),
        });
    }
}

impl AgentUi for SocketUi {
    fn model_delta(&mut self, text: &str) {
        self.emit(UiEventMsg::Delta(text.to_string()));
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
    fn tokens(&mut self, input: u64, output: u64) {
        self.emit(UiEventMsg::Tokens { input, output });
    }
    fn final_message(&mut self, message: &str) {
        self.emit(UiEventMsg::Final(message.to_string()));
    }
    fn notice(&mut self, msg: &str) {
        self.emit(UiEventMsg::Notice(msg.to_string()));
    }
    fn ask_user(&mut self, _question: &str) -> String {
        // Routed over the socket in a later milestone; no approver yet.
        String::new()
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
        let j = inner.journal.lock().unwrap();
        let rx = inner.live.subscribe();
        let len = j.len;
        let replay = read_journal_slice(&j.path, since, len);
        (rx, len, replay)
    };

    let info = inner.info.lock().unwrap().clone();
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
                    if matches!(msg, ClientMsg::Detach) {
                        break;
                    }
                    let _ = cmd_tx.send(msg);
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
}
