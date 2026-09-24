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
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cowboy_core::daemonproto::{
    AskChoice, ClientMsg, PendingPrompt, ServerMsg, SessionInfo, SessionStatus, UiEventMsg,
};
use cowboy_core::netproto::{encode_line, ApprovalDetail, ApprovalScope, Verdict};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex as AsyncMutex};

use super::ui::AgentUi;

/// Hard cap on how long a parked gateway connection waits for a verdict before
/// failing closed. A gateway connection is blocked awaiting this answer, so it
/// must never hang indefinitely.
const APPROVAL_TIMEOUT: Duration =
    Duration::from_secs(cowboy_core::netproto::APPROVAL_TIMEOUT_SECS);

/// How long an already-published prompt survives with no attached client.
const RECONNECT_GRACE: Duration = Duration::from_secs(30);

/// How long `ask_user` waits for a human answer before giving up (returns "").
const ASK_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Copy)]
struct PromptTimeouts {
    reconnect_grace: Duration,
    ask_absolute: Duration,
    approval_absolute: Duration,
}

impl Default for PromptTimeouts {
    fn default() -> Self {
        Self {
            reconnect_grace: RECONNECT_GRACE,
            ask_absolute: ASK_TIMEOUT,
            approval_absolute: APPROVAL_TIMEOUT,
        }
    }
}

/// Live broadcast item: a server message (journaled `Event`s plus control
/// messages like `Ask`/`Approval`/`Ended`).
type Live = ServerMsg;

struct PendingAsk {
    question: String,
    choices: Vec<AskChoice>,
    reply: std::sync::mpsc::Sender<String>,
}

impl PendingAsk {
    fn descriptor(&self, id: u64) -> PendingPrompt {
        PendingPrompt::Ask {
            id,
            question: self.question.clone(),
            options: labels(&self.choices),
            choices: self.choices.clone(),
        }
    }
}

/// The labels alone — what an older client, which knows no `choices`, renders.
fn labels(choices: &[AskChoice]) -> Vec<String> {
    choices.iter().map(|c| c.label.clone()).collect()
}

struct PendingApproval {
    dest: String,
    detail: Option<ApprovalDetail>,
    reply: oneshot::Sender<(Verdict, ApprovalScope)>,
}

impl PendingApproval {
    fn descriptor(&self, id: u64) -> PendingPrompt {
        PendingPrompt::Approval {
            id,
            dest: self.dest.clone(),
            detail: self.detail.clone(),
        }
    }
}

struct DisconnectGrace {
    attachment_epoch: u64,
    disconnected_since: Option<std::time::Instant>,
}

impl DisconnectGrace {
    fn new(inner: &Inner) -> Self {
        Self {
            attachment_epoch: inner.attachment_epoch.load(Ordering::Acquire),
            disconnected_since: None,
        }
    }

    /// True after one continuous zero-client period reaches `grace`.
    /// Any attachment advances the epoch and resets that period, even if the
    /// client disconnects again between two waiter polls.
    fn expired(&mut self, inner: &Inner, grace: Duration) -> bool {
        let epoch = inner.attachment_epoch.load(Ordering::Acquire);
        if epoch != self.attachment_epoch {
            self.attachment_epoch = epoch;
            self.disconnected_since = None;
        }
        if inner.attached.load(Ordering::Relaxed) > 0 {
            self.disconnected_since = None;
            return false;
        }
        self.disconnected_since
            .get_or_insert_with(std::time::Instant::now)
            .elapsed()
            >= grace
    }
}

struct Journal {
    file: std::fs::File,
    path: PathBuf,
    len: u64,
    /// Snapshot metadata and prompt state share this publication lock. A new
    /// subscriber therefore sees each prompt either in its snapshot or live.
    info: SessionInfo,
    base_status: SessionStatus,
    blocked: bool,
    pending_asks: HashMap<u64, PendingAsk>,
    pending_approvals: HashMap<u64, PendingApproval>,
    /// First append failure. Once set, no later event may touch the file: a
    /// rollback can itself fail, leaving bytes after the last committed record.
    poisoned: Option<String>,
}

fn effective_status(publication: &Journal) -> SessionStatus {
    if !publication.pending_approvals.is_empty() {
        SessionStatus::AwaitingApproval
    } else if !publication.pending_asks.is_empty() {
        SessionStatus::AwaitingInput
    } else if publication.blocked {
        SessionStatus::Blocked
    } else {
        publication.base_status
    }
}

fn pending_prompts(publication: &Journal) -> Vec<PendingPrompt> {
    let mut prompts = publication
        .pending_asks
        .iter()
        .map(|(&id, pending)| pending.descriptor(id))
        .chain(
            publication
                .pending_approvals
                .iter()
                .map(|(&id, pending)| pending.descriptor(id)),
        )
        .collect::<Vec<_>>();
    prompts.sort_by_key(|prompt| match prompt {
        PendingPrompt::Ask { id, .. } | PendingPrompt::Approval { id, .. } => *id,
    });
    prompts
}

struct Inner {
    /// Guards journal append + sequence allocation + publication. Keeping the
    /// broadcast send inside this short synchronous critical section makes live
    /// delivery order exactly match committed file order.
    journal: std::sync::Mutex<Journal>,
    live: broadcast::Sender<Live>,
    /// Wakes the worker so an unwritable journal terminates the session through
    /// its normal bounded cancellation path.
    journal_failed: tokio_util::sync::CancellationToken,
    /// Count of currently attached clients.
    attached: AtomicU32,
    /// Advanced on every attachment so a prompt waiter observes even a complete
    /// attach/disconnect cycle between polls and resets continuous grace.
    attachment_epoch: AtomicU64,
    prompt_timeouts: PromptTimeouts,
    /// Set when a client's connection dropped **without** a `Detach` first.
    ///
    /// The distinction is the whole point: a client that detaches on purpose is
    /// saying "keep going, I'll be back", while one whose socket just closed has
    /// gone away — its terminal died, it was killed, or its goodbye was lost. The
    /// second case used to leave the worker waiting for a client that would never
    /// speak again. Cleared when someone attaches, so a reconnect cancels it.
    abandoned: std::sync::atomic::AtomicBool,
    /// Monotonic id for outstanding `Ask`/`Approval` prompts.
    next_req_id: AtomicU64,
    /// Live progress, mirrored from the event stream so the daemon registry
    /// (`cowboy sessions`) can show real numbers without parsing the journal.
    stats: std::sync::Mutex<SessionStats>,
    /// Configured model names and the current one, for `/model` (see
    /// [`crate::agent::commands`]). Set by the worker; empty until it does.
    models: std::sync::Mutex<(Vec<String>, Option<String>)>,
    /// The iteration budget as `(enforced, turns per message)`, for `/budget`.
    budget: std::sync::Mutex<Option<(bool, u32)>>,
    /// Where this session's socket lives, so ending can remove it.
    socket_path: std::path::PathBuf,
    /// Fired by [`SocketUi::end`] to stop the accept loop.
    closed: tokio_util::sync::CancellationToken,
}

/// Snapshot of a session's live progress for the daemon registry.
#[derive(Clone)]
pub struct SessionStats {
    pub status: SessionStatus,
    pub turn: u64,
    pub tokens: (u64, u64),
    pub diffstat: String,
    pub running_command: Option<String>,
    /// Set while the session has declared itself blocked.
    pub blocked_reason: Option<String>,
    /// What the session is about: its first real user message (see
    /// [`session_topic`]). Reported so a session started without a task still
    /// gets a title in `cowboy sessions` and the web UI.
    pub topic: Option<String>,
}

impl Default for SessionStats {
    fn default() -> Self {
        Self {
            status: SessionStatus::Starting,
            turn: 0,
            tokens: (0, 0),
            diffstat: String::new(),
            running_command: None,
            blocked_reason: None,
            topic: None,
        }
    }
}

/// Longest topic kept, in chars. The registry holds one per session, and a list
/// row only shows a line or two anyway.
const TOPIC_MAX_CHARS: usize = 200;

/// A session title from a user message: whitespace collapsed and capped at
/// [`TOPIC_MAX_CHARS`]. `None` for an empty message or a slash command (`/go`,
/// `/accept`), which says nothing about what the session is for.
pub fn session_topic(message: &str) -> Option<String> {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.starts_with('/') {
        return None;
    }
    let mut topic: String = collapsed.chars().take(TOPIC_MAX_CHARS).collect();
    if topic.len() < collapsed.len() {
        topic.push('…');
    }
    Some(topic)
}

/// The `SessionInfo` a `Snapshot` carries: the bind-time registration with the
/// live progress from `stats` laid over it. The registration alone still says
/// `tokens: (0,0)`, `turn: 0`, no diffstat — so a client attaching mid-session
/// would show startup numbers until the next event happened to correct them.
/// `status` is left alone: the caller has just set it from the publication
/// state, which is authoritative (it includes pending prompts).
fn snapshot_info(registered: &SessionInfo, stats: &SessionStats) -> SessionInfo {
    let mut info = registered.clone();
    info.turn = stats.turn;
    info.tokens = stats.tokens;
    info.diffstat = stats.diffstat.clone();
    info.running_command = stats.running_command.clone();
    info.blocked_reason = stats.blocked_reason.clone();
    if info.task.is_none() {
        info.task = stats.topic.clone();
    }
    info
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
        Self::bind_with_timeouts(socket_path, journal_path, info, PromptTimeouts::default()).await
    }

    async fn bind_with_timeouts(
        socket_path: &Path,
        journal_path: &Path,
        info: SessionInfo,
        prompt_timeouts: PromptTimeouts,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ClientMsg>)> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(journal_path)
            .with_context(|| format!("opening journal {}", journal_path.display()))?;
        // Validate through the same descriptor used for every later append and
        // replay. The worktree path is writable, so reopening it would let a rename
        // substitute a different history while this worker still writes the old
        // inode.
        let byte_len = file
            .metadata()
            .with_context(|| format!("reading journal metadata {}", journal_path.display()))?
            .len();
        let len = read_journal_records_from(&file, journal_path, 0, None, byte_len)?.len() as u64;

        // Owner-only, in an owner-only directory. A peer on this socket can inject
        // messages into the agent's conversation and answer outstanding
        // network-approval prompts — answering those *is* the `ask` policy gate — so
        // it is as privileged as the daemon socket.
        let listener = crate::localsock::bind(socket_path)
            .with_context(|| format!("binding session socket {}", socket_path.display()))?;

        let initial_status = info.status;
        let (live, _) = broadcast::channel(4096);
        let inner = Arc::new(Inner {
            journal: std::sync::Mutex::new(Journal {
                file,
                path: journal_path.to_path_buf(),
                len,
                info,
                base_status: match initial_status {
                    SessionStatus::AwaitingApproval
                    | SessionStatus::AwaitingInput
                    | SessionStatus::Blocked => SessionStatus::Running,
                    status => status,
                },
                blocked: initial_status == SessionStatus::Blocked,
                pending_asks: HashMap::new(),
                pending_approvals: HashMap::new(),
                poisoned: None,
            }),
            live,
            journal_failed: tokio_util::sync::CancellationToken::new(),
            attached: AtomicU32::new(0),
            attachment_epoch: AtomicU64::new(0),
            prompt_timeouts,
            abandoned: std::sync::atomic::AtomicBool::new(false),
            next_req_id: AtomicU64::new(0),
            stats: std::sync::Mutex::new(SessionStats {
                status: initial_status,
                ..SessionStats::default()
            }),
            models: std::sync::Mutex::new((Vec::new(), None)),
            budget: std::sync::Mutex::new(None),
            socket_path: socket_path.to_path_buf(),
            closed: tokio_util::sync::CancellationToken::new(),
        });

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let server_inner = inner.clone();
        tokio::spawn(async move {
            accept_loop(listener, server_inner, cmd_tx).await;
        });

        Ok((Self { inner }, cmd_rx))
    }

    /// Record the configured models for `/model`.
    pub fn set_models(&self, names: Vec<String>, current: Option<String>) {
        *self
            .inner
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (names, current);
    }

    /// Record the iteration budget's state, for `/budget`.
    pub fn set_budget(&self, enforced: bool, limit: u32) {
        *self
            .inner
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((enforced, limit));
    }

    /// Record a successful model switch, so `/model` reports the live one.
    pub fn set_current_model(&self, name: &str) {
        self.inner
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = Some(name.to_string());
    }

    /// Expand a session slash command from any client into control messages, and
    /// forward them as if the client had sent them. Returns the reply for the asking
    /// client alone — usage, a listing, a report, or "unknown command" — so a
    /// mistyped command is answered rather than sent to the model.
    fn run_command(&self, text: &str, cmd_tx: &mpsc::UnboundedSender<ClientMsg>) -> Option<String> {
        use crate::agent::commands::{expand, CommandCtx, Expansion};
        let (root, workstream) = {
            let j = self
                .inner
                .journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (j.info.root.clone(), j.info.workstream_id.is_some())
        };
        let (models, current) = self
            .inner
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let budget = *self
            .inner
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ctx = CommandCtx {
            root: &root,
            workstream,
            models: &models,
            current_model: current.as_deref(),
            budget,
        };
        match expand(text, &ctx) {
            Some(Expansion::Send(msgs)) => {
                for m in msgs {
                    let _ = cmd_tx.send(m);
                }
                None
            }
            Some(Expansion::Notice(n)) => Some(n),
            None => {
                let name = text.split_whitespace().next().unwrap_or("");
                Some(match crate::agent::help::nearest(name) {
                    Some(c) => format!("unknown command /{name} — did you mean /{c}?"),
                    None => format!("unknown command /{name}"),
                })
            }
        }
    }

    /// Number of currently attached clients.
    pub fn attached(&self) -> u32 {
        self.inner
            .attached
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Generation advanced on every attachment, including short-lived reconnects.
    pub(crate) fn attachment_epoch(&self) -> u64 {
        self.inner.attachment_epoch.load(Ordering::Acquire)
    }

    /// Has the session been left with nobody driving it?
    ///
    /// True once a client's connection dropped without detaching and no client has
    /// attached since. This is what lets the worker stop waiting on a goodbye it may
    /// never receive: `End` travels over the socket, so *every* way the client can
    /// die without sending it — SIGKILL, a closed terminal, a lost race in the
    /// client's own shutdown — used to strand the worker (and with it the sandbox
    /// holder and the daemon) until someone noticed it in `ps`.
    pub fn abandoned(&self) -> bool {
        self.attached() == 0
            && self
                .inner
                .abandoned
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Replace snapshot metadata while retaining the authoritative live status.
    pub fn set_info(&self, mut info: SessionInfo) {
        let mut publication = self
            .inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        info.status = effective_status(&publication);
        publication.info = info;
    }

    /// Current effective lifecycle status.
    pub fn status(&self) -> SessionStatus {
        let publication = self
            .inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        effective_status(&publication)
    }

    /// Set the worker's underlying lifecycle phase. Pending approvals, asks, and
    /// a declared block take precedence, and the effective status is both
    /// snapshotted and broadcast as a transient, nonjournaled control.
    pub fn set_status(&self, status: SessionStatus) {
        let mut publication = self
            .inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publication.base_status = status;
        self.publish_status_locked(&mut publication);
    }

    fn set_blocked(&self, blocked: bool) {
        let mut publication = self
            .inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publication.blocked = blocked;
        self.publish_status_locked(&mut publication);
    }

    /// Lock order is publication (`journal`) then `stats`. No path may acquire
    /// them in the opposite order, and no async or channel-blocking work belongs
    /// in this critical section.
    fn publish_status_locked(&self, publication: &mut Journal) {
        let status = effective_status(publication);
        publication.info.status = status;
        let mut stats = self
            .inner
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stats.status != status {
            stats.status = status;
            let _ = self.inner.live.send(ServerMsg::Status(status));
        }
    }

    fn resolve_ask(&self, id: u64, answer: String) -> bool {
        let pending = {
            let mut publication = self
                .inner
                .journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(pending) = publication.pending_asks.remove(&id) else {
                return false;
            };
            let _ = self.inner.live.send(ServerMsg::AskResolved { id });
            self.publish_status_locked(&mut publication);
            pending
        };
        let _ = pending.reply.send(answer);
        true
    }

    fn resolve_approval(&self, id: u64, verdict: (Verdict, ApprovalScope)) -> bool {
        let pending = {
            let mut publication = self
                .inner
                .journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(pending) = publication.pending_approvals.remove(&id) else {
                return false;
            };
            let _ = self.inner.live.send(ServerMsg::ApprovalResolved { id });
            self.publish_status_locked(&mut publication);
            pending
        };
        let _ = pending.reply.send(verdict);
        true
    }

    /// Journal + broadcast a display event (worker-originated events like
    /// `DiffStat`/`Title`/`Processes`/`TurnDone` use this directly).
    pub fn emit(&self, event: UiEventMsg) {
        let encoded = serde_json::to_vec(&event).map(|mut line| {
            line.push(b'\n');
            line
        });
        let mut j = self
            .inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if j.poisoned.is_some() {
            return;
        }

        let line = match encoded {
            Ok(line) => line,
            Err(error) => {
                self.poison_journal(&mut j, format!("serializing event: {error}"));
                return;
            }
        };
        let seq = j.len;
        let offset = match j.file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.poison_journal(&mut j, format!("reading append position: {error}"));
                return;
            }
        };
        if let Err(error) = j.file.write_all(&line).and_then(|()| j.file.flush()) {
            let rollback = j.file.set_len(offset).err();
            let detail = match rollback {
                Some(rollback) => format!(
                    "appending event: {error}; rollback to byte {offset} also failed: {rollback}"
                ),
                None => format!("appending event: {error}; rolled back to byte {offset}"),
            };
            self.poison_journal(&mut j, detail);
            return;
        }

        // Sequence allocation and broadcast happen only after the complete record
        // has been written and flushed. Keep publication under the same lock so two
        // concurrent emitters cannot broadcast in the opposite order from commit.
        j.len += 1;
        self.track(&event);
        let _ = self.inner.live.send(ServerMsg::Event { seq, event });
    }

    fn poison_journal(&self, journal: &mut Journal, detail: String) {
        if journal.poisoned.is_some() {
            return;
        }
        let reason = format!(
            "session journal failed at {}: {detail}",
            journal.path.display()
        );
        tracing::error!(path = %journal.path.display(), error = %detail, "session journal poisoned");
        journal.poisoned = Some(reason.clone());
        // This terminal signal is deliberately out-of-band: trying to journal a
        // journal failure would append into possible corruption. Like Ask/Approval,
        // nonjournaled control messages cannot be reconstructed during lag recovery.
        let _ = self.inner.live.send(ServerMsg::Ended { reason });
        self.inner.journal_failed.cancel();
    }

    /// Wait until the first journal failure and return its stable diagnostic.
    pub async fn wait_for_journal_failure(&self) -> String {
        self.inner.journal_failed.cancelled().await;
        self.journal_failure()
            .unwrap_or_else(|| "session journal failed".into())
    }

    /// The first journal failure, if appends have been permanently disabled.
    pub fn journal_failure(&self) -> Option<String> {
        self.inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .poisoned
            .clone()
    }

    /// Mirror progress-bearing events into `stats` for the daemon registry.
    fn track(&self, event: &UiEventMsg) {
        let mut s = self
            .inner
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match event {
            UiEventMsg::UserMessage(m) if s.topic.is_none() => s.topic = session_topic(m),
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

    /// Tell attached clients the session is over, then **stop being attachable**.
    ///
    /// The second half is the part that was missing, and it is the whole reason an
    /// ended session could be attached to again: notifying current clients left the
    /// accept loop running and the socket file on disk, so a client that connected
    /// afterwards was accepted by a worker that had already finished — appearing to
    /// join a live session, then hanging when the worker exited. The daemon's registry
    /// said `Completed`, but a socket path is enough to attach with.
    ///
    /// Removing the socket first means a late attach fails immediately with "no such
    /// file", which is both true and actionable, instead of connecting to nothing.
    pub fn end(&self, reason: &str) {
        let (asks, approvals) = {
            let publication = self
                .inner
                .journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                publication.pending_asks.keys().copied().collect::<Vec<_>>(),
                publication
                    .pending_approvals
                    .keys()
                    .copied()
                    .collect::<Vec<_>>(),
            )
        };
        for id in asks {
            self.resolve_ask(id, String::new());
        }
        for id in approvals {
            self.resolve_approval(id, (Verdict::Deny, ApprovalScope::Once));
        }
        let _ = self.inner.live.send(ServerMsg::Ended {
            reason: reason.to_string(),
        });
        // Order matters: unlink before cancelling, so there is no window in which the
        // loop has stopped accepting while the path still looks connectable.
        let _ = std::fs::remove_file(&self.inner.socket_path);
        self.inner.closed.cancel();
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

    /// Ask attached clients to approve a network destination. A new request with
    /// no client fails closed immediately. Once published, it survives a continuous
    /// zero-client period for the reconnect grace; any attachment resets that grace.
    /// The first reply wins, and every published outcome emits `ApprovalResolved`.
    /// Absolute or grace expiry returns `Deny`/`Once`.
    ///
    /// `detail` is display-only structure for the modal (see
    /// [`cowboy_core::netproto::ApprovalDetail`]); it never affects the verdict.
    pub async fn request_approval(
        &self,
        dest: String,
        detail: Option<ApprovalDetail>,
    ) -> (Verdict, ApprovalScope) {
        if self.attached() == 0 {
            return (Verdict::Deny, ApprovalScope::Once);
        }
        let id = self.inner.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = oneshot::channel();
        {
            let mut publication = self
                .inner
                .journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            publication.pending_approvals.insert(
                id,
                PendingApproval {
                    dest: dest.clone(),
                    detail: detail.clone(),
                    reply: tx,
                },
            );
            let _ = self
                .inner
                .live
                .send(ServerMsg::Approval { id, dest, detail });
            self.publish_status_locked(&mut publication);
        }

        let timeouts = self.inner.prompt_timeouts;
        let absolute = tokio::time::Instant::now() + timeouts.approval_absolute;
        let mut disconnect = DisconnectGrace::new(&self.inner);
        loop {
            tokio::select! {
                result = &mut rx => {
                    return result.unwrap_or((Verdict::Deny, ApprovalScope::Once));
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    let fallback = (Verdict::Deny, ApprovalScope::Once);
                    if (disconnect.expired(&self.inner, timeouts.reconnect_grace)
                        || tokio::time::Instant::now() >= absolute)
                        && self.resolve_approval(id, fallback)
                    {
                        return fallback;
                    }
                }
            }
        }
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
    fn context_usage(&mut self, u: &crate::agent::ui::ContextUsage) {
        self.emit(UiEventMsg::ContextUsage {
            used: u.used,
            budget: u.budget,
            window: u.window,
            reserve: u.reserve,
            top: u.top.clone(),
        });
    }
    fn cost(&mut self, usd: f64) {
        self.emit(UiEventMsg::Cost(usd));
    }
    fn blocked(&mut self, reason: Option<&str>) {
        self.set_blocked(reason.is_some());
        self.emit(UiEventMsg::Blocked(reason.map(str::to_string)));
    }
    fn plan(&mut self, steps: &[(String, String)]) {
        self.emit(UiEventMsg::Plan(steps.to_vec()));
    }
    fn subagent_pending(&mut self, label: &str, model: &str, id: &str) {
        self.emit(UiEventMsg::SubagentPending {
            label: label.to_string(),
            model: model.to_string(),
            id: id.to_string(),
        });
    }
    fn subagent_started(&mut self, label: &str, model: &str, id: &str) {
        self.emit(UiEventMsg::SubagentStarted {
            label: label.to_string(),
            model: model.to_string(),
            id: id.to_string(),
        });
    }
    fn subagent_done(&mut self, label: &str, ok: bool, id: &str) {
        self.emit(UiEventMsg::SubagentDone {
            label: label.to_string(),
            ok,
            id: id.to_string(),
        });
    }
    fn jobs_changed(&mut self, jobs: &[cowboy_core::daemonproto::JobInfo]) {
        self.emit(UiEventMsg::JobsChanged(jobs.to_vec()));
    }
    fn queue_changed(&mut self, pending: &[String]) {
        self.emit(UiEventMsg::QueueChanged {
            pending: pending.to_vec(),
        });
    }
    fn steering(&mut self, text: &str) {
        self.emit(UiEventMsg::SteerDelivered(text.to_string()));
    }
    fn final_message(&mut self, message: &str) {
        self.emit(UiEventMsg::Final(message.to_string()));
    }
    fn notice(&mut self, msg: &str) {
        self.emit(UiEventMsg::Notice(msg.to_string()));
    }
    fn can_ask_user(&self) -> bool {
        self.attached() > 0
    }
    fn ask_user(&mut self, question: &str, options: &[String]) -> String {
        let choices: Vec<AskChoice> = options.iter().map(|o| o.as_str().into()).collect();
        self.ask_user_rich(question, &choices)
    }
    fn ask_user_rich(&mut self, question: &str, choices: &[AskChoice]) -> String {
        // A new prompt with nobody attached has never been published, so it fails
        // immediately according to the non-interactive/subagent contract.
        if self.attached() == 0 {
            return String::new();
        }
        let id = self.inner.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut publication = self
                .inner
                .journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            publication.pending_asks.insert(
                id,
                PendingAsk {
                    question: question.to_string(),
                    choices: choices.to_vec(),
                    reply: tx,
                },
            );
            let _ = self.inner.live.send(ServerMsg::Ask {
                id,
                question: question.to_string(),
                options: labels(choices),
                choices: choices.to_vec(),
            });
            self.publish_status_locked(&mut publication);
        }
        // The agent loop blocks here for the answer. That is safe, and deliberately so
        // rather than by luck: `AgentUi` is a sync trait called from the middle of the
        // loop, and the loop is driven by the worker's top-level `block_on` future —
        // the process main thread — not by a task on the runtime's worker pool. The
        // reply arrives on the socket accept loop, which *is* a spawned task, so it
        // still gets scheduled even on a single-vCPU host where the pool has one
        // thread. Verified by
        // `tests::an_ask_is_answerable_on_a_single_worker_runtime`.
        //
        // The one real consequence is that the `select!` in `cmd::worker` cannot poll
        // its other arms while an ask is outstanding, so an orphan-cancellation lands
        // when the ask resolves rather than during it. Acceptable: a session should not
        // be torn down halfway through asking the user a question.
        //
        // First reply wins. Once published, a prompt survives a continuous
        // zero-client period for reconnect; any attachment resets that period.
        let timeouts = self.inner.prompt_timeouts;
        let deadline = std::time::Instant::now() + timeouts.ask_absolute;
        let mut disconnect = DisconnectGrace::new(&self.inner);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(answer) => return answer,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return String::new(),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if (disconnect.expired(&self.inner, timeouts.reconnect_grace)
                        || std::time::Instant::now() >= deadline)
                        && self.resolve_ask(id, String::new())
                    {
                        return String::new();
                    }
                }
            }
        }
    }
}

async fn accept_loop(
    listener: UnixListener,
    inner: Arc<Inner>,
    cmd_tx: mpsc::UnboundedSender<ClientMsg>,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            // Stop accepting the moment the session ends, so nothing can attach to a
            // worker that is on its way out.
            _ = inner.closed.cancelled() => {
                tracing::debug!("session ended; no longer accepting clients");
                return;
            }
            a = listener.accept() => a,
        };
        let Ok((stream, _)) = accepted else {
            continue;
        };
        // From the kernel, not from anything the client sends: a peer here can answer
        // network-approval prompts, so admitting the wrong uid would hand the `ask`
        // gate to another local user.
        if !crate::localsock::peer_is_ours(&stream) {
            continue;
        }
        let inner = inner.clone();
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            inner
                .attached
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            inner
                .attachment_epoch
                .fetch_add(1, std::sync::atomic::Ordering::Release);
            // A fresh client cancels any earlier abandonment: someone is driving again.
            inner
                .abandoned
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let graceful = serve_client(stream, inner.clone(), cmd_tx)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "client connection failed");
                    false
                });
            let remaining = inner
                .attached
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
                .saturating_sub(1);
            if !graceful && remaining == 0 {
                inner
                    .abandoned
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            tracing::debug!(graceful, remaining, "client connection closed");
        });
    }
}

/// Serve one client until it goes away. Returns whether it left **gracefully** —
/// that is, said `Detach` rather than simply vanishing.
async fn serve_client(
    stream: UnixStream,
    inner: Arc<Inner>,
    cmd_tx: mpsc::UnboundedSender<ClientMsg>,
) -> Result<bool> {
    let (r, w) = stream.into_split();
    let writer = Arc::new(AsyncMutex::new(w));
    let mut reader = BufReader::new(r);

    // First line: optional Hello{since_seq, read_only}.
    //
    // `read_only` is honoured HERE, per connection. It used to be parsed and
    // dropped, with filtering left to the client — so anything speaking the
    // protocol (the web UI, a future client, a buggy bridge) could attach
    // "watch-only" and still drive, end, or `/accept` the session. Enforce it at
    // the boundary that actually owns the session instead of trusting the peer.
    let mut first = String::new();
    let (since, read_only) = if reader.read_line(&mut first).await? == 0 {
        // Connected and said nothing: a probe, not a driver. Not an abandonment.
        return Ok(true);
    } else {
        match serde_json::from_str::<ClientMsg>(first.trim()) {
            Ok(ClientMsg::Hello {
                since_seq,
                read_only,
                ..
            }) => (since_seq.unwrap_or(0), read_only),
            // A non-Hello first line is treated as input + a full replay.
            Ok(other) => {
                let _ = cmd_tx.send(other);
                (0, false)
            }
            Err(_) => (0, false),
        }
    };

    // Atomically subscribe to live and snapshot the committed journal boundary.
    // Parsing uses `read_at` on a duplicate of the pinned descriptor after the
    // synchronous lock is released, so a large replay cannot block emitters and
    // no pathname replacement can substitute another history.
    let (rx, journal_len, replay_file, replay_path, replay_bytes, info, prompts) = {
        let mut j = inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = j.file.try_clone().context("duplicating live journal")?;
        let byte_len = j
            .file
            .metadata()
            .context("reading live journal metadata")?
            .len();
        j.info.status = effective_status(&j);
        let prompts = pending_prompts(&j);
        // `j.info` is the bind-time registration; the live numbers are in
        // `stats`. Lock order: publication (held) then stats.
        let info = {
            let stats = inner
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot_info(&j.info, &stats)
        };
        (
            inner.live.subscribe(),
            j.len,
            file,
            j.path.clone(),
            byte_len,
            info,
            prompts,
        )
    };
    let replay = read_journal_slice(&replay_file, &replay_path, since, journal_len, replay_bytes)?;

    send(
        &writer,
        &ServerMsg::Snapshot {
            info,
            journal_len,
            pending_prompts: prompts,
        },
    )
    .await?;
    for (seq, event) in replay {
        send(&writer, &ServerMsg::Event { seq, event }).await?;
    }

    // Pump live events and client input concurrently. If the publication pump
    // requests a resync (for example after broadcast lag), dropping the reader
    // closes this connection so the client must reconnect through an authoritative
    // Snapshot instead of continuing after possibly skipped controls.
    let live_writer = writer.clone();
    let live_inner = inner.clone();
    let mut live =
        tokio::spawn(async move { pump_live(rx, live_writer, live_inner, journal_len).await });
    let read_inner = inner.clone();
    let reply_writer = writer.clone();
    let mut input = tokio::spawn(async move {
        let mut graceful = false;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(msg) = serde_json::from_str::<ClientMsg>(line.trim()) {
                        match msg {
                            ClientMsg::Detach => {
                                graceful = true;
                                break;
                            }
                            ClientMsg::ApprovalReply { .. } | ClientMsg::AskReply { .. }
                                if read_only =>
                            {
                                tracing::debug!("dropping a prompt reply from a read-only client");
                            }
                            ClientMsg::ApprovalReply { id, verdict, scope } => {
                                SocketUi {
                                    inner: read_inner.clone(),
                                }
                                .resolve_approval(id, (verdict, scope));
                            }
                            ClientMsg::AskReply { id, answer } => {
                                SocketUi {
                                    inner: read_inner.clone(),
                                }
                                .resolve_ask(id, answer);
                            }
                            ClientMsg::Command(text) if !read_only => {
                                let reply = SocketUi {
                                    inner: read_inner.clone(),
                                }
                                .run_command(&text, &cmd_tx);
                                if let Some(text) = reply {
                                    let msg = ServerMsg::CommandReply { text };
                                    if send(&reply_writer, &msg).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            other if read_only => {
                                tracing::debug!(
                                    msg = ?std::mem::discriminant(&other),
                                    "dropping a mutating message from a read-only client"
                                );
                            }
                            other => {
                                let _ = cmd_tx.send(other);
                            }
                        }
                    }
                }
            }
        }
        graceful
    });

    let graceful = tokio::select! {
        result = &mut input => result.unwrap_or(false),
        result = &mut live => {
            if let Ok(Err(error)) = result {
                tracing::warn!(%error, "client event stream requires reconnect");
            }
            false
        }
    };
    input.abort();
    live.abort();
    Ok(graceful)
}

/// Pump committed events contiguously to one client.
///
/// A lag notification or forward sequence jump is repaired from the journal. Any
/// queued Event that overlaps the repaired range is then discarded, so the client
/// sees each sequence exactly once. Nonjournaled control messages have no sequence
/// and cannot be reconstructed if the broadcast receiver lagged past them.
async fn pump_live(
    mut rx: broadcast::Receiver<Live>,
    writer: Arc<AsyncMutex<tokio::net::unix::OwnedWriteHalf>>,
    inner: Arc<Inner>,
    mut next_seq: u64,
) -> Result<()> {
    loop {
        match rx.recv().await {
            Ok(ServerMsg::Event { seq, event }) => {
                if seq > next_seq {
                    recover_committed(&writer, &inner, &mut next_seq).await?;
                    anyhow::bail!(
                        "live sequence jumped; reconnect required to resnapshot transient controls"
                    );
                }
                if seq < next_seq {
                    continue;
                }
                if seq != next_seq {
                    anyhow::bail!(
                        "event sequence jumped from {next_seq} to {seq} beyond committed journal"
                    );
                }
                send(&writer, &ServerMsg::Event { seq, event }).await?;
                next_seq += 1;
            }
            Ok(msg) => {
                let ended = matches!(msg, ServerMsg::Ended { .. });
                send(&writer, &msg).await?;
                if ended {
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::debug!(
                    skipped,
                    next_seq,
                    "client lagged; recovering journal before reconnect"
                );
                recover_committed(&writer, &inner, &mut next_seq).await?;
                anyhow::bail!("client lagged; reconnect required to resnapshot transient controls");
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn recover_committed(
    writer: &AsyncMutex<tokio::net::unix::OwnedWriteHalf>,
    inner: &Inner,
    next_seq: &mut u64,
) -> Result<()> {
    let (file, path, committed_len, committed_bytes) = {
        let journal = inner
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            journal
                .file
                .try_clone()
                .context("duplicating live journal")?,
            journal.path.clone(),
            journal.len,
            journal
                .file
                .metadata()
                .context("reading live journal metadata")?
                .len(),
        )
    };
    let missing = read_journal_slice(&file, &path, *next_seq, committed_len, committed_bytes)?;
    for (seq, event) in missing {
        if seq != *next_seq {
            anyhow::bail!(
                "journal recovery expected sequence {}, found {seq}",
                *next_seq
            );
        }
        send(writer, &ServerMsg::Event { seq, event }).await?;
        *next_seq += 1;
    }
    if *next_seq != committed_len {
        anyhow::bail!(
            "journal recovery ended at sequence {}, expected {committed_len}",
            *next_seq
        );
    }
    Ok(())
}

/// Read and validate journaled events `[since..len)` (0-based seq = line number).
fn read_journal_slice(
    file: &std::fs::File,
    path: &Path,
    since: u64,
    len: u64,
    byte_len: u64,
) -> Result<Vec<(u64, UiEventMsg)>> {
    read_journal_records_from(file, path, since.min(len), Some(len), byte_len)
}

/// Read a complete journal, rejecting malformed JSON and unterminated records.
pub(crate) fn read_journal(path: &Path) -> Result<Vec<UiEventMsg>> {
    Ok(read_journal_records(path, 0, None)?
        .into_iter()
        .map(|(_, event)| event)
        .collect())
}

fn read_journal_records(
    path: &Path,
    since: u64,
    end: Option<u64>,
) -> Result<Vec<(u64, UiEventMsg)>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening journal {}", path.display()))?;
    let byte_len = file
        .metadata()
        .with_context(|| format!("reading journal metadata {}", path.display()))?
        .len();
    read_journal_records_from(&file, path, since, end, byte_len)
}

/// A bounded, offset-independent view of an open file.
///
/// Using `read_at` avoids changing the append descriptor's shared offset and lets
/// multiple clients scan one pinned inode concurrently without allocating the
/// complete journal.
struct FileSlice<'a> {
    file: &'a std::fs::File,
    offset: u64,
    end: u64,
}

impl Read for FileSlice<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.offset >= self.end || buf.is_empty() {
            return Ok(0);
        }
        let remaining = self.end - self.offset;
        let limit = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let read = self.file.read_at(&mut buf[..limit], self.offset)?;
        self.offset += read as u64;
        Ok(read)
    }
}

/// Parse a stable byte snapshot through an already-open journal descriptor.
///
/// `byte_len` is captured under the append lock, so later complete appends or a
/// failed append rollback cannot change this reader's committed boundary.
fn read_journal_records_from(
    file: &std::fs::File,
    path: &Path,
    since: u64,
    end: Option<u64>,
    byte_len: u64,
) -> Result<Vec<(u64, UiEventMsg)>> {
    let source = FileSlice {
        file,
        offset: 0,
        end: byte_len,
    };
    let mut reader = std::io::BufReader::new(source);
    let mut records = Vec::new();
    let mut seq = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = std::io::BufRead::read_until(&mut reader, b'\n', &mut line)
            .with_context(|| format!("reading journal {} at sequence {seq}", path.display()))?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            anyhow::bail!(
                "journal {} has an incomplete record at sequence {seq}",
                path.display()
            );
        }
        line.pop();
        let event = serde_json::from_slice::<UiEventMsg>(&line).with_context(|| {
            format!(
                "parsing journal {} record at sequence {seq}",
                path.display()
            )
        })?;
        if seq >= since && end.is_none_or(|limit| seq < limit) {
            records.push((seq, event));
        }
        seq += 1;
        if end.is_some_and(|limit| seq >= limit) {
            break;
        }
    }
    if let Some(end) = end {
        if seq < end {
            anyhow::bail!(
                "journal {} ended at sequence {seq}, expected {end}",
                path.display()
            );
        }
    }
    Ok(records)
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

    #[test]
    fn session_topic_collapses_caps_and_skips_commands() {
        assert_eq!(
            session_topic("  fix\n\tthe  bug "),
            Some("fix the bug".into())
        );
        assert_eq!(session_topic("/go"), None);
        assert_eq!(session_topic("   "), None);
        let long = session_topic(&"é".repeat(TOPIC_MAX_CHARS + 5)).unwrap();
        assert_eq!(long.chars().count(), TOPIC_MAX_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    fn info() -> SessionInfo {
        SessionInfo {
            id: "t".into(),
            root: "/tmp/x".into(),
            task: None,
            status: SessionStatus::Running,
            pid: None,
            branch: None,
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

    async fn read_msg(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> ServerMsg {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    /// A client attaching mid-session gets the live numbers in its Snapshot, not
    /// the bind-time registration (`tokens: (0,0)`, `turn: 0`, no task).
    #[tokio::test]
    async fn snapshot_carries_live_stats_and_topic() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();

        ui.emit(UiEventMsg::UserMessage("  fix the\nbug ".into()));
        ui.emit(UiEventMsg::Tokens {
            input: 120,
            output: 34,
        });
        ui.emit(UiEventMsg::DiffStat("+3 -1".into()));
        ui.emit(UiEventMsg::TurnDone);
        ui.emit(UiEventMsg::CommandStart("cargo test".into()));
        ui.emit(UiEventMsg::Blocked(Some("needs a key".into())));

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        send_client(
            &mut w,
            &ClientMsg::Hello {
                since_seq: Some(6),
                read_only: true,
            },
        )
        .await;
        match read_msg(&mut reader).await {
            ServerMsg::Snapshot { info, .. } => {
                assert_eq!(info.tokens, (120, 34));
                assert_eq!(info.turn, 1);
                assert_eq!(info.diffstat, "+3 -1");
                assert_eq!(info.running_command.as_deref(), Some("cargo test"));
                assert_eq!(info.blocked_reason.as_deref(), Some("needs a key"));
                assert_eq!(
                    info.task.as_deref(),
                    Some("fix the bug"),
                    "topic fills task"
                );
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    /// An explicit task is never replaced by the topic.
    #[test]
    fn snapshot_info_keeps_an_explicit_task() {
        let mut registered = info();
        registered.task = Some("the task".into());
        let stats = SessionStats {
            topic: Some("first message".into()),
            ..SessionStats::default()
        };
        assert_eq!(
            snapshot_info(&registered, &stats).task.as_deref(),
            Some("the task")
        );
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

    /// The session socket is at least as privileged as the daemon's: a peer on it can
    /// inject messages into the conversation and **answer outstanding
    /// network-approval prompts**, which is the `ask` policy gate. So no other local
    /// user may reach it. It used to be bound at the process umask in a directory
    /// created with `create_dir_all`'s default.
    #[tokio::test]
    async fn the_session_socket_is_not_reachable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("run");
        let sock = dir.join("s.sock");
        let (_ui, _cmd_rx) = SocketUi::bind(&sock, &tmp.path().join("events.jsonl"), info())
            .await
            .unwrap();

        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&sock), 0o600, "answering an `ask` must stay ours");
        assert_eq!(mode(&dir), 0o700);
    }

    /// A client that attached read-only may watch, but must not drive the session.
    /// Enforcement lives here (worker-side), not in the client: the flag used to be
    /// parsed and discarded, so any peer speaking the protocol could attach
    /// "watch-only" and still end or redirect the session.
    #[tokio::test]
    async fn read_only_client_cannot_drive_the_session() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        let (mut ui, mut cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        w.write_all(
            encode_line(&ClientMsg::Hello {
                since_seq: None,
                read_only: true,
            })
            .as_bytes(),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();
        match read_msg(&mut reader).await {
            ServerMsg::Snapshot { .. } => {}
            other => panic!("expected Snapshot, got {other:?}"),
        }

        // Mutating messages from this connection must be dropped, not forwarded.
        for msg in [
            ClientMsg::Message("do something destructive".into()),
            ClientMsg::End,
            ClientMsg::SwitchModel("expensive".into()),
        ] {
            w.write_all(encode_line(&msg).as_bytes()).await.unwrap();
        }
        w.flush().await.unwrap();

        // It can still WATCH: a live event proves the connection is alive and
        // serving, and gives the reader time to have processed the writes above.
        ui.tool_use("still streaming");
        match read_msg(&mut reader).await {
            ServerMsg::Event {
                event: UiEventMsg::ToolUse(s),
                ..
            } => assert_eq!(s, "still streaming"),
            other => panic!("read-only client must still receive events, got {other:?}"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "no message from a read-only client may reach the agent loop"
        );
    }

    /// A slash command from any client is expanded by the worker: `/go` becomes the
    /// same `PlanMode(false)` + approval turn the TUI sends, and a typo is answered to
    /// that client alone rather than sent to the model. A read-only client's command
    /// is dropped like any other mutation.
    #[tokio::test]
    async fn slash_commands_are_expanded_by_the_worker_for_every_client() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        let (_ui, mut cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let hello = ClientMsg::Hello {
            since_seq: None,
            read_only: false,
        };
        w.write_all(encode_line(&hello).as_bytes()).await.unwrap();
        assert!(matches!(
            read_msg(&mut reader).await,
            ServerMsg::Snapshot { .. }
        ));

        w.write_all(encode_line(&ClientMsg::Command("go ship it".into())).as_bytes())
            .await
            .unwrap();
        let mut got = Vec::new();
        for _ in 0..2 {
            let m = tokio::time::timeout(std::time::Duration::from_secs(5), cmd_rx.recv())
                .await
                .unwrap()
                .unwrap();
            got.push(m);
        }
        assert_eq!(
            got,
            [
                ClientMsg::PlanMode(false),
                ClientMsg::Message(crate::agent::commands::go_prompt("ship it")),
            ]
        );

        w.write_all(encode_line(&ClientMsg::Command("gp".into())).as_bytes())
            .await
            .unwrap();
        match read_msg(&mut reader).await {
            ServerMsg::CommandReply { text } => assert!(text.contains("/go"), "{text}"),
            other => panic!("expected a reply to the asking client, got {other:?}"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "a typo must not reach the agent loop"
        );
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

    async fn wait_for_attached(ui: &SocketUi, expected: u32) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while ui.attached() != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    fn test_timeouts(
        reconnect_grace: Duration,
        ask_absolute: Duration,
        approval_absolute: Duration,
    ) -> PromptTimeouts {
        PromptTimeouts {
            reconnect_grace,
            ask_absolute,
            approval_absolute,
        }
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
            ui.request_approval("example.com:443".into(), None).await,
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
        let verdict =
            tokio::spawn(async move { ask_ui.request_approval("h:443".into(), None).await });

        let id = match read_msg(&mut reader).await {
            ServerMsg::Approval { id, dest, .. } => {
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
        // Other clients are told to dismiss the now-decided modal. A lifecycle
        // update may have been queued immediately after publication.
        loop {
            match read_msg(&mut reader).await {
                ServerMsg::ApprovalResolved { id: rid } => {
                    assert_eq!(rid, id);
                    break;
                }
                ServerMsg::Status(_) => {}
                other => panic!("expected ApprovalResolved, got {other:?}"),
            }
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
        loop {
            match read_msg(&mut reader).await {
                ServerMsg::AskResolved { id: resolved } => {
                    assert_eq!(resolved, id);
                    break;
                }
                ServerMsg::Status(_) => {}
                other => panic!("expected AskResolved, got {other:?}"),
            }
        }
    }

    /// The same property on a runtime with a **single** worker thread, with `ask_user`
    /// called inline rather than handed to `spawn_blocking`.
    ///
    /// This is how the agent loop calls it: `AgentUi` is a sync trait invoked from the
    /// middle of the loop. A review flagged the blocking `recv_timeout` as able to
    /// starve the runtime — it cannot, because the loop runs on the worker's top-level
    /// `block_on` future while the reply arrives on a *spawned* accept-loop task, and
    /// tokio sizes the worker pool from the host's core count. This pins that, since
    /// the property is not obvious from reading either side alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn an_ask_is_answerable_on_a_single_worker_runtime() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &tmp.path().join("events.jsonl"), info())
            .await
            .unwrap();

        // The only client lives on a plain OS thread with its own runtime, so nothing
        // about the reply path can borrow the server runtime's single worker.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let sock2 = sock.clone();
        let replier = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let (mut r, mut w) = attach_client(&sock2).await;
                // Attached *and* subscribed (attach_client waits for the Snapshot), so
                // the Ask cannot be broadcast before anyone is listening.
                ready_tx.send(()).unwrap();
                loop {
                    if let ServerMsg::Ask { id, .. } = read_msg(&mut r).await {
                        send_client(
                            &mut w,
                            &ClientMsg::AskReply {
                                id,
                                answer: "yes".into(),
                            },
                        )
                        .await;
                        return;
                    }
                }
            });
        });
        ready_rx.recv().unwrap();

        let mut ask_ui = ui.clone();
        assert_eq!(
            ask_ui.ask_user("continue?", &[]),
            "yes",
            "the reply task must still be schedulable while the loop blocks"
        );
        replier.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_contains_sorted_full_pending_descriptors() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &tmp.path().join("events.jsonl"), info())
            .await
            .unwrap();
        let (mut first_reader, _first_writer) = attach_client(&sock).await;

        let mut ask_ui = ui.clone();
        let answer = tokio::task::spawn_blocking(move || {
            ask_ui.ask_user("continue?", &["yes".into(), "no".into()])
        });
        let ask_id = loop {
            if let ServerMsg::Ask { id, .. } = read_msg(&mut first_reader).await {
                break id;
            }
        };

        let detail = ApprovalDetail {
            kind: cowboy_core::netproto::ApprovalKind::Credential,
            rows: vec![("mount".into(), "/secret".into())],
            note: Some("display only".into()),
        };
        let approval_ui = ui.clone();
        let expected_detail = detail.clone();
        let verdict = tokio::spawn(async move {
            approval_ui
                .request_approval("credential mount".into(), Some(detail))
                .await
        });
        let approval_id = loop {
            if let ServerMsg::Approval { id, .. } = read_msg(&mut first_reader).await {
                break id;
            }
        };

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        send_client(
            &mut w,
            &ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            },
        )
        .await;
        match read_msg(&mut reader).await {
            ServerMsg::Snapshot {
                info,
                pending_prompts,
                ..
            } => {
                assert_eq!(info.status, SessionStatus::AwaitingApproval);
                assert_eq!(
                    pending_prompts,
                    vec![
                        PendingPrompt::Ask {
                            id: ask_id,
                            question: "continue?".into(),
                            options: vec!["yes".into(), "no".into()],
                            choices: vec!["yes".into(), "no".into()],
                        },
                        PendingPrompt::Approval {
                            id: approval_id,
                            dest: "credential mount".into(),
                            detail: Some(expected_detail),
                        },
                    ]
                );
            }
            other => panic!("expected authoritative Snapshot, got {other:?}"),
        }

        send_client(
            &mut w,
            &ClientMsg::AskReply {
                id: ask_id,
                answer: "yes".into(),
            },
        )
        .await;
        send_client(
            &mut w,
            &ClientMsg::ApprovalReply {
                id: approval_id,
                verdict: Verdict::Allow,
                scope: ApprovalScope::Session,
            },
        )
        .await;
        assert_eq!(answer.await.unwrap(), "yes");
        assert_eq!(
            verdict.await.unwrap(),
            (Verdict::Allow, ApprovalScope::Session)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn published_ask_survives_reconnects_and_each_attachment_resets_grace() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let timeouts = test_timeouts(
            Duration::from_millis(600),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        let (ui, _cmd_rx) =
            SocketUi::bind_with_timeouts(&sock, &tmp.path().join("events.jsonl"), info(), timeouts)
                .await
                .unwrap();
        let (mut reader, writer) = attach_client(&sock).await;
        let mut ask_ui = ui.clone();
        let answer = tokio::task::spawn_blocking(move || ask_ui.ask_user("still there?", &[]));
        let id = loop {
            if let ServerMsg::Ask { id, .. } = read_msg(&mut reader).await {
                break id;
            }
        };
        drop(reader);
        drop(writer);
        wait_for_attached(&ui, 0).await;
        tokio::time::sleep(Duration::from_millis(350)).await;

        // This brief attachment resets grace even though it disconnects again.
        let (reader, writer) = attach_client(&sock).await;
        drop(reader);
        drop(writer);
        wait_for_attached(&ui, 0).await;
        tokio::time::sleep(Duration::from_millis(350)).await;

        // Total disconnected time exceeds grace, but neither continuous period does.
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        send_client(
            &mut w,
            &ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            },
        )
        .await;
        match read_msg(&mut reader).await {
            ServerMsg::Snapshot {
                pending_prompts, ..
            } => assert_eq!(
                pending_prompts,
                vec![PendingPrompt::Ask {
                    id,
                    question: "still there?".into(),
                    options: Vec::new(),
                    choices: Vec::new(),
                }]
            ),
            other => panic!("expected reconnect Snapshot, got {other:?}"),
        }
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
    async fn reconnect_grace_expiry_resolves_with_safe_fallbacks() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let timeouts = test_timeouts(
            Duration::from_millis(250),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        let (ui, _cmd_rx) =
            SocketUi::bind_with_timeouts(&sock, &tmp.path().join("events.jsonl"), info(), timeouts)
                .await
                .unwrap();
        let (reader, writer) = attach_client(&sock).await;
        let mut live = ui.inner.live.subscribe();

        let mut ask_ui = ui.clone();
        let answer = tokio::task::spawn_blocking(move || ask_ui.ask_user("answer?", &[]));
        let ask_id = loop {
            if let ServerMsg::Ask { id, .. } = live.recv().await.unwrap() {
                break id;
            }
        };
        drop(reader);
        drop(writer);
        wait_for_attached(&ui, 0).await;
        assert_eq!(answer.await.unwrap(), "");
        loop {
            if matches!(live.recv().await.unwrap(), ServerMsg::AskResolved { id } if id == ask_id) {
                break;
            }
        }

        let (reader, writer) = attach_client(&sock).await;
        let verdict_ui = ui.clone();
        let verdict = tokio::spawn(async move {
            verdict_ui
                .request_approval("example.com:443".into(), None)
                .await
        });
        let approval_id = loop {
            if let ServerMsg::Approval { id, .. } = live.recv().await.unwrap() {
                break id;
            }
        };
        drop(reader);
        drop(writer);
        wait_for_attached(&ui, 0).await;
        assert_eq!(verdict.await.unwrap(), (Verdict::Deny, ApprovalScope::Once));
        loop {
            if matches!(live.recv().await.unwrap(), ServerMsg::ApprovalResolved { id } if id == approval_id)
            {
                break;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn absolute_timeouts_resolve_published_prompts_with_safe_fallbacks() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let timeouts = test_timeouts(
            Duration::from_secs(5),
            Duration::from_millis(250),
            Duration::from_millis(250),
        );
        let (ui, _cmd_rx) =
            SocketUi::bind_with_timeouts(&sock, &tmp.path().join("events.jsonl"), info(), timeouts)
                .await
                .unwrap();
        let (_reader, _writer) = attach_client(&sock).await;
        let mut live = ui.inner.live.subscribe();

        let mut ask_ui = ui.clone();
        let answer = tokio::task::spawn_blocking(move || ask_ui.ask_user("answer?", &[]));
        let ask_id = loop {
            if let ServerMsg::Ask { id, .. } = live.recv().await.unwrap() {
                break id;
            }
        };
        assert_eq!(answer.await.unwrap(), "");
        loop {
            if matches!(live.recv().await.unwrap(), ServerMsg::AskResolved { id } if id == ask_id) {
                break;
            }
        }

        let verdict_ui = ui.clone();
        let verdict = tokio::spawn(async move {
            verdict_ui
                .request_approval("example.com:443".into(), None)
                .await
        });
        let approval_id = loop {
            if let ServerMsg::Approval { id, .. } = live.recv().await.unwrap() {
                break id;
            }
        };
        assert_eq!(verdict.await.unwrap(), (Verdict::Deny, ApprovalScope::Once));
        loop {
            if matches!(live.recv().await.unwrap(), ServerMsg::ApprovalResolved { id } if id == approval_id)
            {
                break;
            }
        }
    }

    #[tokio::test]
    async fn effective_status_preserves_base_and_only_broadcasts_changes() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &tmp.path().join("events.jsonl"), info())
            .await
            .unwrap();
        let mut live = ui.inner.live.subscribe();

        assert_eq!(ui.status(), SessionStatus::Running);
        ui.set_status(SessionStatus::Running);
        assert!(matches!(
            live.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        ui.set_status(SessionStatus::Idle);
        assert_eq!(
            live.try_recv().unwrap(),
            ServerMsg::Status(SessionStatus::Idle)
        );

        ui.set_blocked(true);
        assert_eq!(ui.status(), SessionStatus::Blocked);
        assert_eq!(
            live.try_recv().unwrap(),
            ServerMsg::Status(SessionStatus::Blocked)
        );
        ui.set_status(SessionStatus::Running);
        assert!(matches!(
            live.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        let (ask_tx, _ask_rx) = std::sync::mpsc::channel();
        let (approval_tx, _approval_rx) = oneshot::channel();
        {
            let mut publication = ui.inner.journal.lock().unwrap();
            publication.pending_asks.insert(
                1,
                PendingAsk {
                    question: "question".into(),
                    choices: Vec::new(),
                    reply: ask_tx,
                },
            );
            ui.publish_status_locked(&mut publication);
            publication.pending_approvals.insert(
                2,
                PendingApproval {
                    dest: "destination".into(),
                    detail: None,
                    reply: approval_tx,
                },
            );
            ui.publish_status_locked(&mut publication);
            publication.pending_approvals.remove(&2);
            ui.publish_status_locked(&mut publication);
            publication.pending_asks.remove(&1);
            ui.publish_status_locked(&mut publication);
        }
        for expected in [
            SessionStatus::AwaitingInput,
            SessionStatus::AwaitingApproval,
            SessionStatus::AwaitingInput,
            SessionStatus::Blocked,
        ] {
            assert_eq!(live.try_recv().unwrap(), ServerMsg::Status(expected));
        }

        ui.set_blocked(false);
        assert_eq!(ui.status(), SessionStatus::Running);
        assert_eq!(
            live.try_recv().unwrap(),
            ServerMsg::Status(SessionStatus::Running)
        );
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut writer) = stream.into_split();
        let mut reader = BufReader::new(r);
        send_client(
            &mut writer,
            &ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            },
        )
        .await;
        match read_msg(&mut reader).await {
            ServerMsg::Snapshot { info, .. } => {
                assert_eq!(info.status, SessionStatus::Running)
            }
            other => panic!("expected current status in Snapshot, got {other:?}"),
        }
        assert!(matches!(
            live.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_emitters_broadcast_in_journal_commit_order() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();
        let mut rx = ui.inner.live.subscribe();
        let ui = Arc::new(ui);

        std::thread::scope(|scope| {
            for emitter in 0..32 {
                let ui = ui.clone();
                scope.spawn(move || ui.emit(UiEventMsg::ToolUse(emitter.to_string())));
            }
        });

        let committed = read_journal(&journal).unwrap();
        assert_eq!(committed.len(), 32);
        for (seq, expected) in committed.into_iter().enumerate() {
            assert_eq!(
                rx.try_recv().unwrap(),
                ServerMsg::Event {
                    seq: seq as u64,
                    event: expected,
                }
            );
        }
    }

    #[tokio::test]
    async fn lag_replays_missing_range_and_discards_queued_overlap() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();
        let rx = ui.inner.live.subscribe();

        // The receiver is deliberately not polled until the fixed 4096-slot
        // broadcast ring has overflowed.
        const EVENTS: u64 = 4_100;
        for seq in 0..EVENTS {
            ui.emit(UiEventMsg::ToolUse(seq.to_string()));
        }

        let (server, client) = UnixStream::pair().unwrap();
        let (_server_r, server_w) = server.into_split();
        let (client_r, _client_w) = client.into_split();
        let writer = Arc::new(AsyncMutex::new(server_w));
        let inner = ui.inner.clone();
        let pump = tokio::spawn(pump_live(rx, writer, inner, 0));
        let mut reader = BufReader::new(client_r);

        for expected in 0..EVENTS {
            match read_msg(&mut reader).await {
                ServerMsg::Event {
                    seq,
                    event: UiEventMsg::ToolUse(value),
                } => {
                    assert_eq!(seq, expected);
                    assert_eq!(value, expected.to_string());
                }
                other => panic!("expected contiguous event {expected}, got {other:?}"),
            }
        }
        let error = pump.await.unwrap().unwrap_err();
        assert!(
            error.to_string().contains("reconnect required"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn sequence_jump_recovers_then_discards_the_overlapping_event() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();
        for seq in 0..3 {
            ui.emit(UiEventMsg::ToolUse(seq.to_string()));
        }

        // Feed the pump only seq 2, simulating a receiver that observes a forward
        // jump without Tokio first reporting Lagged.
        let (tx, rx) = broadcast::channel(8);
        tx.send(ServerMsg::Event {
            seq: 2,
            event: UiEventMsg::ToolUse("2".into()),
        })
        .unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let (_server_r, server_w) = server.into_split();
        let (client_r, _client_w) = client.into_split();
        let writer = Arc::new(AsyncMutex::new(server_w));
        let inner = ui.inner.clone();
        let pump = tokio::spawn(pump_live(rx, writer, inner, 0));
        let mut reader = BufReader::new(client_r);

        for expected in 0..3 {
            match read_msg(&mut reader).await {
                ServerMsg::Event { seq, .. } => assert_eq!(seq, expected),
                other => panic!("expected recovered event {expected}, got {other:?}"),
            }
        }
        let error = pump.await.unwrap().unwrap_err();
        assert!(
            error.to_string().contains("reconnect required"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn append_failure_poisoning_is_permanent_and_out_of_band() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();
        let mut rx = ui.inner.live.subscribe();
        {
            let mut state = ui.inner.journal.lock().unwrap();
            // A device that fails every write (ENOSPC); macOS has none, so there a
            // handle opened read-only stands in (EBADF).
            #[cfg(target_os = "linux")]
            let failing = std::fs::OpenOptions::new().write(true).open("/dev/full");
            #[cfg(not(target_os = "linux"))]
            let failing = std::fs::File::open(&journal);
            state.file = failing.unwrap();
        }

        ui.emit(UiEventMsg::ToolUse("fails".into()));
        let reason = ui.wait_for_journal_failure().await;
        assert!(reason.contains("session journal failed"), "{reason}");
        assert!(matches!(
            rx.recv().await.unwrap(),
            ServerMsg::Ended { reason: sent } if sent == reason
        ));

        ui.emit(UiEventMsg::ToolUse("must not append".into()));
        assert_eq!(std::fs::metadata(&journal).unwrap().len(), 0);
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn live_replay_stays_on_the_bound_inode_after_path_replacement() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        let moved = tmp.path().join("original-events.jsonl");
        let (ui, _cmd_rx) = SocketUi::bind(&sock, &journal, info()).await.unwrap();
        for value in ["original-zero", "original-one", "original-two"] {
            ui.emit(UiEventMsg::ToolUse(value.into()));
        }

        std::fs::rename(&journal, &moved).unwrap();
        let mut replacement =
            serde_json::to_vec(&UiEventMsg::ToolUse("replacement".into())).unwrap();
        replacement.push(b'\n');
        std::fs::write(&journal, replacement).unwrap();

        // Initial attach replays the inode opened by bind, not the new pathname.
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
        match read_msg(&mut reader).await {
            ServerMsg::Snapshot { journal_len, .. } => assert_eq!(journal_len, 3),
            other => panic!("expected Snapshot, got {other:?}"),
        }
        for (seq, expected) in ["original-zero", "original-one", "original-two"]
            .into_iter()
            .enumerate()
        {
            match read_msg(&mut reader).await {
                ServerMsg::Event {
                    seq: got,
                    event: UiEventMsg::ToolUse(value),
                } => {
                    assert_eq!(got, seq as u64);
                    assert_eq!(value, expected);
                }
                other => panic!("expected pinned replay event {seq}, got {other:?}"),
            }
        }

        // Lag/jump recovery uses the same pinned descriptor.
        let (server, client) = UnixStream::pair().unwrap();
        let (_server_r, server_w) = server.into_split();
        let (client_r, _client_w) = client.into_split();
        let writer = AsyncMutex::new(server_w);
        let mut next_seq = 0;
        recover_committed(&writer, &ui.inner, &mut next_seq)
            .await
            .unwrap();
        assert_eq!(next_seq, 3);
        let mut recovery_reader = BufReader::new(client_r);
        for (seq, expected) in ["original-zero", "original-one", "original-two"]
            .into_iter()
            .enumerate()
        {
            match read_msg(&mut recovery_reader).await {
                ServerMsg::Event {
                    seq: got,
                    event: UiEventMsg::ToolUse(value),
                } => {
                    assert_eq!(got, seq as u64);
                    assert_eq!(value, expected);
                }
                other => panic!("expected pinned recovery event {seq}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn bind_rejects_an_incomplete_existing_record() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        std::fs::write(&journal, serde_json::to_vec(&UiEventMsg::TurnDone).unwrap()).unwrap();

        let error = match SocketUi::bind(&sock, &journal, info()).await {
            Ok(_) => panic!("incomplete journal unexpectedly accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("incomplete record"), "{error:#}");
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
