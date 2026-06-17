//! Wire types for the `cowboyd` daemon architecture, shared by the `cowboy`
//! client, the `cowboyd` daemon, and the headless session worker.
//!
//! Three protocols, all framed with [`crate::netproto::encode_line`]
//! (newline-delimited JSON):
//!
//! * **Daemon control API** — [`DaemonRequest`] / [`DaemonResponse`]: the
//!   client and the worker talk to the daemon (registry, leases, worktrees,
//!   supervision). Request/response, `id`-correlated so a connection can
//!   pipeline.
//! * **Worker → client** — [`ServerMsg`]: the live event stream (the
//!   serializable image of the in-process `UiEvent`), plus id-tagged `Ask` /
//!   `Approval` prompts whose replies travel back as [`ClientMsg`].
//! * **Client → worker** — [`ClientMsg`].
//!
//! The session event journal (`events.jsonl`) is a sequence of [`UiEventMsg`]
//! lines; the 0-based line number is the event's `seq`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::netproto::{ApprovalScope, Verdict};

/// A session identifier: `{now_ms}-{worker_pid}` (the pid is the worker's, which
/// is what the daemon needs for liveness checks).
pub type SessionId = String;

/// How a session holds its worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    /// One writable session per worktree (the default).
    Exclusive,
    /// A read-only follower; never conflicts with anything.
    ReadOnly,
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Idle,
    AwaitingApproval,
    AwaitingInput,
    /// The session has declared it cannot proceed (see `blocked_reason`); still
    /// live and attachable, just waiting on an external input/dependency.
    Blocked,
    Completed,
    Failed,
    Stale,
}

impl SessionStatus {
    /// True once the session can no longer be attached live.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Stale
        )
    }
}

/// Registry record for a session, as reported by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_sock: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_path: Option<PathBuf>,
    #[serde(default)]
    pub lease_mode: Option<LeaseMode>,
    #[serde(default)]
    pub started_ms: u64,
    #[serde(default)]
    pub last_heartbeat_ms: u64,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub tokens: (u64, u64),
    #[serde(default)]
    pub attached_clients: u32,
    #[serde(default)]
    pub diffstat: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_command: Option<String>,
    /// Why the session is blocked (set while `status == Blocked`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// The ranch this session belongs to (set for Ranch workstream sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranch_id: Option<String>,
    /// The workstream within the ranch this session is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workstream_id: Option<String>,
}

/// A cowboy-created git worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    pub branch: String,
    pub path: PathBuf,
    pub status: SessionStatus,
}

/// A structured cross-session message routed by the daemon. This is NOT a free
/// agent↔agent chat channel — it carries coordination events (a Ranch
/// coordinator routes these in a later stage).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusEvent {
    /// A human message to a session.
    UserMessage(String),
    /// A message from another session.
    SessionMessage(String),
    /// A status update worth surfacing.
    StatusUpdate(String),
    /// The sender became blocked.
    Blocked(String),
    /// A handoff is available to consume.
    HandoffAvailable { artifact_id: String },
    /// An artifact was published.
    ArtifactPublished { artifact_id: String, kind: String },
    /// The sender wants attention.
    AttentionRequested(String),
}

/// A delivered bus message: who sent it, when, and what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusMessage {
    pub from: String,
    pub ts_ms: u64,
    pub event: BusEvent,
}

/// Who a [`BusEvent`] is addressed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MsgTarget {
    /// A specific session's inbox.
    Session(SessionId),
    /// Every other known session.
    All,
}

/// Where a client should attach for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachTarget {
    /// The worker is alive — connect to this per-session socket.
    Live { worker_sock: PathBuf },
    /// The worker is gone — render the journal read-only from disk.
    Replay {
        journal_path: PathBuf,
        status: SessionStatus,
    },
}

// ---------------------------------------------------------------------------
// Daemon control API
// ---------------------------------------------------------------------------

/// An `id`-correlated request to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub id: u64,
    pub req: DaemonReq,
}

/// An `id`-correlated response from the daemon (echoes the request `id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub id: u64,
    pub resp: DaemonResp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonReq {
    Ping,
    StartSession {
        root: PathBuf,
        #[serde(default)]
        task: Option<String>,
        mode: LeaseMode,
        /// Steal a *stale* lease on this worktree (never a live one).
        #[serde(default)]
        force: bool,
        /// Continue a prior session's conversation: load its transcript as the
        /// new session's starting history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
        /// Tag the session as a Ranch workstream (set by `cowboy ranch start`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ranch_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workstream_id: Option<String>,
    },
    ListSessions {
        #[serde(default)]
        root: Option<PathBuf>,
    },
    GetSession {
        id: SessionId,
    },
    AttachSession {
        id: SessionId,
    },
    DetachSession {
        id: SessionId,
    },
    // worker -> daemon
    RegisterWorker {
        info: SessionInfo,
    },
    UpdateSession {
        id: SessionId,
        status: SessionStatus,
        #[serde(default)]
        turn: u64,
        #[serde(default)]
        tokens: (u64, u64),
        #[serde(default)]
        diffstat: String,
        #[serde(default)]
        attached_clients: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        running_command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocked_reason: Option<String>,
    },
    Heartbeat {
        id: SessionId,
        seq: u64,
    },
    CompleteSession {
        id: SessionId,
    },
    FailSession {
        id: SessionId,
        error: String,
    },
    // leases + worktrees
    AcquireLease {
        key: PathBuf,
        session: SessionId,
        mode: LeaseMode,
    },
    ReleaseLease {
        key: PathBuf,
        session: SessionId,
    },
    ListWorktrees {
        repo: PathBuf,
    },
    CreateWorktree {
        repo: PathBuf,
        branch: String,
        #[serde(default)]
        path: Option<PathBuf>,
    },
    CleanupStale {
        #[serde(default)]
        dry_run: bool,
    },
    /// Deliver a structured message to a session inbox (or all sessions).
    SendMessage {
        to: MsgTarget,
        from: SessionId,
        event: BusEvent,
    },
    /// Read (and optionally drain) a session's inbox.
    GetInbox {
        session: SessionId,
        #[serde(default)]
        drain: bool,
    },
    /// Sign off on the ranch workstream a session is running: mark it complete,
    /// promote its artifacts, and advance the plan (launch newly-unblocked
    /// workstreams). Sent by the worker when the user runs `/accept` in-session.
    AcceptWorkstream {
        session: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonResp {
    Pong {
        version: String,
        pid: u32,
        sessions: usize,
    },
    Started {
        id: SessionId,
        worker_sock: PathBuf,
    },
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    Session {
        info: SessionInfo,
    },
    Attach {
        target: AttachTarget,
    },
    Detached,
    Registered,
    Updated,
    Completed,
    Failed,
    LeaseGranted {
        key: PathBuf,
    },
    LeaseDenied {
        key: PathBuf,
        held_by: SessionInfo,
    },
    Worktrees {
        list: Vec<WorktreeInfo>,
    },
    WorktreeCreated {
        path: PathBuf,
        branch: String,
    },
    CleanedUp {
        reclaimed: Vec<SessionId>,
        leases_released: Vec<PathBuf>,
    },
    /// A message was delivered to `delivered` inbox(es).
    Sent {
        delivered: usize,
    },
    /// The contents of a session's inbox.
    Inbox {
        messages: Vec<BusMessage>,
    },
    /// A workstream was signed off and the plan advanced.
    Accepted,
    Err {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Worker <-> client
// ---------------------------------------------------------------------------

/// The serializable image of the in-process display events (everything the
/// agent emits to a UI except the channel-carrying `Ask`/`Approval`). One of
/// these per line is the `events.jsonl` journal format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiEventMsg {
    Delta(String),
    /// The model's streamed "thinking" (reasoning), shown dimmed and never
    /// folded into the answer.
    Reasoning(String),
    ModelDone,
    CommandStart(String),
    CommandOutput(String),
    CommandEnd {
        code: i32,
        output: String,
    },
    ToolUse(String),
    /// A unified diff of a file the agent created or edited.
    FileDiff {
        path: String,
        diff: String,
    },
    Final(String),
    Notice(String),
    NetEvent(String),
    DiffStat(String),
    Tokens {
        input: u64,
        output: u64,
    },
    /// Running estimated session spend in USD.
    Cost(f64),
    /// The session is blocked (`Some(reason)`) or unblocked (`None`).
    Blocked(Option<String>),
    /// The agent's working plan: ordered (step, status) pairs.
    Plan(Vec<(String, String)>),
    Title(String),
    Processes(Vec<(String, String)>),
    /// A crew subagent was dispatched (routing label + resolved model).
    SubagentStarted {
        label: String,
        model: String,
    },
    /// A crew subagent finished (`ok` = produced a result).
    SubagentDone {
        label: String,
        ok: bool,
    },
    TurnDone,
}

/// Worker → client messages over the per-session socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // `Snapshot` is sent once per attach; a wire enum, size is moot
pub enum ServerMsg {
    /// Sent once per attach (not journaled): session metadata + journal length
    /// at the moment of subscription.
    Snapshot { info: SessionInfo, journal_len: u64 },
    /// A journaled display event with its sequence number (= journal line).
    Event { seq: u64, event: UiEventMsg },
    /// A pending question for the user; reply with [`ClientMsg::AskReply`].
    /// `options` (possibly empty) are suggested choices for a pick-list.
    Ask {
        id: u64,
        question: String,
        #[serde(default)]
        options: Vec<String>,
    },
    /// A pending network approval; reply with [`ClientMsg::ApprovalReply`].
    Approval { id: u64, dest: String },
    /// A previously broadcast `Approval` has been decided (by another client or
    /// on timeout); clients should dismiss its modal.
    ApprovalResolved { id: u64 },
    /// The session changed lifecycle state.
    Status(SessionStatus),
    /// Terminal: the worker is shutting down; the connection will close.
    Ended { reason: String },
}

/// What an interrupt from the client should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptKind {
    /// Cancel the current turn; the session continues.
    Turn,
    /// Cancel the turn and return to idle for a new instruction.
    Instruct,
    /// End the whole session.
    End,
}

/// Client → worker messages over the per-session socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientMsg {
    /// First line after connecting: replay from `since_seq` (None = all).
    Hello {
        #[serde(default)]
        since_seq: Option<u64>,
        #[serde(default)]
        read_only: bool,
    },
    Message(String),
    AskReply {
        id: u64,
        answer: String,
    },
    ApprovalReply {
        id: u64,
        verdict: Verdict,
        scope: ApprovalScope,
    },
    SwitchModel(String),
    /// Turn plan mode on/off: while on, the agent proposes a plan and the loop
    /// blocks file edits until it's turned off (the user approves with `/go`).
    PlanMode(bool),
    /// Sign off on the ranch workstream this session is running (the user typed
    /// `/accept`): the worker asks the daemon to complete the workstream + advance
    /// the plan, then ends the session.
    Accept {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Interrupt {
        kind: InterruptKind,
    },
    /// Disconnect but leave the session running.
    Detach,
    /// End the session.
    End,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netproto::encode_line;

    fn roundtrip<T>(v: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let line = encode_line(v);
        assert!(line.ends_with('\n'));
        let back: T = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v, &back);
    }

    #[test]
    fn daemon_request_response_roundtrip() {
        roundtrip(&DaemonRequest {
            id: 1,
            req: DaemonReq::StartSession {
                root: "/home/me/app".into(),
                task: Some("fix tests".into()),
                mode: LeaseMode::Exclusive,
                force: false,
                resume: None,
                ranch_id: None,
                workstream_id: None,
            },
        });
        roundtrip(&DaemonRequest {
            id: 2,
            req: DaemonReq::Ping,
        });
        roundtrip(&DaemonRequest {
            id: 3,
            req: DaemonReq::SendMessage {
                to: MsgTarget::Session("s1".into()),
                from: "user".into(),
                event: BusEvent::UserMessage("hi".into()),
            },
        });
        roundtrip(&DaemonResponse {
            id: 3,
            resp: DaemonResp::Inbox {
                messages: vec![BusMessage {
                    from: "user".into(),
                    ts_ms: 5,
                    event: BusEvent::HandoffAvailable {
                        artifact_id: "a0001".into(),
                    },
                }],
            },
        });
        roundtrip(&DaemonResponse {
            id: 2,
            resp: DaemonResp::Pong {
                version: "0.1.0".into(),
                pid: 42,
                sessions: 3,
            },
        });
        roundtrip(&DaemonResponse {
            id: 3,
            resp: DaemonResp::LeaseDenied {
                key: "/home/me/app".into(),
                held_by: sample_info(),
            },
        });
        roundtrip(&DaemonRequest {
            id: 4,
            req: DaemonReq::AcceptWorkstream {
                session: "123-456".into(),
                note: Some("looks good".into()),
            },
        });
        roundtrip(&DaemonResponse {
            id: 4,
            resp: DaemonResp::Accepted,
        });
    }

    #[test]
    fn server_and_client_messages_roundtrip() {
        roundtrip(&ServerMsg::Event {
            seq: 5,
            event: UiEventMsg::CommandEnd {
                code: 0,
                output: "ok".into(),
            },
        });
        roundtrip(&ServerMsg::Ask {
            id: 1,
            question: "continue?".into(),
            options: vec!["yes".into(), "no".into()],
        });
        roundtrip(&ServerMsg::Snapshot {
            info: sample_info(),
            journal_len: 12,
        });
        roundtrip(&ClientMsg::Hello {
            since_seq: Some(3),
            read_only: false,
        });
        roundtrip(&ClientMsg::ApprovalReply {
            id: 1,
            verdict: Verdict::Allow,
            scope: ApprovalScope::Session,
        });
        roundtrip(&ServerMsg::Approval {
            id: 2,
            dest: "example.com:443".into(),
        });
        roundtrip(&ServerMsg::ApprovalResolved { id: 2 });
        roundtrip(&ClientMsg::Accept {
            note: Some("ship it".into()),
        });
        roundtrip(&ClientMsg::Accept { note: None });
    }

    #[test]
    fn journal_event_roundtrip() {
        roundtrip(&UiEventMsg::Tokens {
            input: 1200,
            output: 340,
        });
        roundtrip(&UiEventMsg::Cost(0.42));
        roundtrip(&UiEventMsg::Blocked(Some("need the contract".into())));
        roundtrip(&UiEventMsg::Blocked(None));
        roundtrip(&UiEventMsg::Plan(vec![
            ("scope the work".into(), "done".into()),
            ("implement".into(), "in_progress".into()),
        ]));
        roundtrip(&UiEventMsg::Processes(vec![(
            "web".into(),
            "running".into(),
        )]));
    }

    fn sample_info() -> SessionInfo {
        SessionInfo {
            id: "123-456".into(),
            root: "/home/me/app".into(),
            task: Some("fix tests".into()),
            status: SessionStatus::Running,
            pid: Some(456),
            branch: Some("main".into()),
            container_name: Some("cowboy-agent-app-deadbeef".into()),
            worker_sock: Some("/run/cowboy/s-123.sock".into()),
            journal_path: Some("/home/me/app/.cowboy/sessions/123-456/events.jsonl".into()),
            lease_mode: Some(LeaseMode::Exclusive),
            started_ms: 1,
            last_heartbeat_ms: 2,
            turn: 1,
            tokens: (10, 20),
            attached_clients: 1,
            diffstat: "Δ 1f +2 -0".into(),
            running_command: None,
            blocked_reason: None,
            ranch_id: None,
            workstream_id: None,
        }
    }
}
