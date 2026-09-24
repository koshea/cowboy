//! The browser-side transcript model: the same shape as the TUI's `App` state,
//! mutated by the wire `ServerMsg`/`UiEventMsg` stream. Kept free of Yew/web-sys
//! so it stays pure and mirrors `apply_wire` 1:1.

use cowboy_proto::daemonproto::{AskChoice, PendingPrompt, ServerMsg, SessionStatus, UiEventMsg};

/// One rendered transcript entry.
#[derive(Clone, PartialEq)]
pub enum Block {
    User(String),
    Agent(String),
    Command {
        cmd: String,
        /// Committed (newline-terminated) output.
        output: String,
        /// The current transient line — a `\r` progress update the producer sends
        /// without a newline. Each one replaces the last, as in a terminal.
        live: String,
        exit: Option<i32>,
    },
    Tool(String),
    Diff {
        path: String,
        diff: String,
    },
    Notice(String),
    Final(String),
}

/// Connection lifecycle, shown as a banner.
#[derive(Clone, PartialEq, Default)]
pub enum ConnState {
    #[default]
    Connecting,
    Live,
    /// The socket dropped (network blip); the client is retrying.
    Reconnecting,
    /// Repeated connection attempts could not reach the session, but no terminal
    /// `Ended` was observed.
    Unavailable,
    /// The session itself ended (terminal — no reconnect).
    Ended(String),
}

/// A pending question or approval awaiting the user.
#[derive(Clone, PartialEq)]
pub struct Ask {
    pub id: u64,
    pub question: String,
    /// The choices, full or (from an older worker) labels only.
    pub options: Vec<AskChoice>,
}

#[derive(Clone, PartialEq)]
pub struct Approval {
    pub id: u64,
    pub dest: String,
    /// Modal title from the prompt's kind, so a credential prompt is not labelled
    /// as a network one.
    pub title: String,
    /// `(label, value)` detail rows; empty when the worker sent none, in which case
    /// `dest` is all there is to show.
    pub rows: Vec<(String, String)>,
    pub note: Option<String>,
    /// A credential grant: per request, never remembered, so no scope is offered.
    pub once_only: bool,
}

impl Approval {
    fn new(id: u64, dest: String, detail: Option<cowboy_proto::netproto::ApprovalDetail>) -> Self {
        use cowboy_proto::netproto::ApprovalKind;
        let (title, rows, note, once_only) = match detail {
            Some(d) => (
                match d.kind {
                    ApprovalKind::Network => "Network request",
                    ApprovalKind::Credential => "Credential access",
                },
                d.rows,
                d.note,
                d.kind == ApprovalKind::Credential,
            ),
            None => ("Approval", Vec::new(), None, false),
        };
        Approval {
            id,
            dest,
            title: title.to_string(),
            rows,
            note,
            once_only,
        }
    }
}

/// A crew subagent shown in the session view; click it to watch its live output.
#[derive(Clone, PartialEq)]
pub struct SubagentStatus {
    pub id: String,
    pub label: String,
    pub model: String,
    /// `true` = planned but waiting for a concurrency permit (per-provider cap);
    /// not yet running. Flips to `false` on the matching `SubagentStarted`.
    pub pending: bool,
    /// `None` = running, `Some(true)` = finished ok, `Some(false)` = failed.
    pub done: Option<bool>,
    /// When the worker has spent its turn grant and is waiting for the foreman to
    /// answer: how many more turns it asked for. `0` when it is not asking.
    pub requested: u32,
    /// Turns spent / granted, when the roster supervises turn budgets.
    pub used: u32,
    pub granted: u32,
    /// Waiting on an answer from the foreman (`asking a question`).
    pub asking: bool,
    /// Wall time so far, as of the last job snapshot.
    pub elapsed_ms: u64,
    /// What it was asked to do.
    pub task: String,
}

/// The latest context-window snapshot (the TUI's `/context`).
#[derive(Clone, PartialEq, Default)]
pub struct Context {
    pub used: u64,
    pub budget: u64,
    pub window: u64,
    pub reserve: u64,
    pub top: Vec<(String, u64)>,
}

impl Context {
    /// Percent of the conversation budget in use (saturating, like the TUI's).
    pub fn percent(&self) -> u64 {
        if self.budget == 0 {
            return if self.used == 0 { 0 } else { 100 };
        }
        (self.used.saturating_mul(100) / self.budget).min(999)
    }
}

/// How many network events the activity panel keeps.
const NET_EVENTS_KEPT: usize = 200;

#[derive(Clone, PartialEq, Default)]
pub struct Model {
    /// What the session is about (its task, or first message) — the header title.
    pub task: Option<String>,
    /// The worker's context title (`~/proj ⎇ branch`), shown under the task.
    pub title: String,
    pub blocks: Vec<Block>,
    /// In-progress (un-committed) model output.
    pub streaming: String,
    /// Streamed reasoning, shown dimmed until the turn commits.
    pub reasoning: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    /// `None` until the first turn reports it.
    pub context: Option<Context>,
    pub diffstat: String,
    pub plan: Vec<(String, String)>,
    pub blocked: Option<String>,
    /// Outstanding questions, oldest first — the worker answers them in id order,
    /// so the oldest is the one shown.
    pub asks: Vec<Ask>,
    /// Outstanding approvals, oldest first.
    pub approvals: Vec<Approval>,
    pub status: Option<SessionStatus>,
    pub conn: ConnState,
    /// Derived from events when the worker hasn't reported a status (an old
    /// worker, a replay); otherwise the status is authoritative — see [`Self::busy`].
    pub running: bool,
    /// Crew subagents dispatched this session, for the watch list.
    pub subagents: Vec<SubagentStatus>,
    /// Input the user queued to run after the current turn.
    pub queued: Vec<String>,
    /// Recent network activity, newest last.
    pub net: Vec<String>,
    /// Background processes as `(name, state)`.
    pub processes: Vec<(String, String)>,
    /// Journal events this client could not parse (a newer worker), skipped.
    pub skipped: u64,
}

impl Model {
    /// Commit any buffered streaming text as an `Agent` block.
    fn commit(&mut self) {
        let text = std::mem::take(&mut self.streaming);
        self.reasoning.clear();
        let text = text.trim_end();
        if !text.is_empty() {
            self.blocks.push(Block::Agent(text.to_string()));
        }
    }

    /// Apply one worker→client message.
    pub fn apply(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Snapshot {
                info,
                pending_prompts,
                ..
            } => {
                self.conn = ConnState::Live;
                // Outstanding prompts reach a (re)connecting client only through
                // the snapshot, and it is authoritative: anything not listed was
                // resolved while we were away. The modals hold one of each kind,
                // so show the oldest, as the worker would answer it first.
                self.asks.clear();
                self.approvals.clear();
                for prompt in pending_prompts {
                    match prompt {
                        PendingPrompt::Ask {
                            id,
                            question,
                            options,
                            choices,
                        } => self.push_ask(Ask {
                            id,
                            question,
                            options: AskChoice::resolve(&options, &choices),
                        }),
                        PendingPrompt::Approval { id, dest, detail } => {
                            self.push_approval(Approval::new(id, dest, detail))
                        }
                    }
                }
                self.status = Some(info.status);
                if info.task.is_some() {
                    self.task = info.task;
                }
                if self.title.is_empty() {
                    self.title = info.id;
                }
                self.tokens_in = info.tokens.0;
                self.tokens_out = info.tokens.1;
                self.diffstat = info.diffstat;
                self.blocked = info.blocked_reason;
            }
            ServerMsg::Event { event, .. } => self.apply_event(event),
            ServerMsg::Ask {
                id,
                question,
                options,
                choices,
            } => self.push_ask(Ask {
                id,
                question,
                options: AskChoice::resolve(&options, &choices),
            }),
            ServerMsg::Approval { id, dest, detail } => {
                self.push_approval(Approval::new(id, dest, detail))
            }
            ServerMsg::AskResolved { id } => self.asks.retain(|a| a.id != id),
            ServerMsg::ApprovalResolved { id } => self.approvals.retain(|a| a.id != id),
            ServerMsg::CommandReply { text } => self.push_notice(text),
            ServerMsg::Status(s) => self.status = Some(s),
            ServerMsg::Ended { reason } => {
                self.commit();
                self.reasoning.clear();
                self.running = false;
                self.conn = ConnState::Ended(reason);
                // Nothing can answer a prompt once the worker is gone.
                self.asks.clear();
                self.approvals.clear();
                // The worker may die before reporting in-flight subagents done;
                // freeze them (pending ones included) so none shows forever live.
                for s in &mut self.subagents {
                    if s.done.is_none() {
                        s.done = Some(false);
                        s.pending = false;
                        s.requested = 0;
                        s.asking = false;
                    }
                }
            }
        }
    }

    /// Show something to this user only (a local command's output, a refusal).
    pub fn push_notice(&mut self, text: String) {
        self.commit();
        self.blocks.push(Block::Notice(text));
    }

    fn push_ask(&mut self, ask: Ask) {
        if !self.asks.iter().any(|a| a.id == ask.id) {
            self.asks.push(ask);
            self.asks.sort_by_key(|a| a.id);
        }
    }

    fn push_approval(&mut self, ap: Approval) {
        if !self.approvals.iter().any(|a| a.id == ap.id) {
            self.approvals.push(ap);
            self.approvals.sort_by_key(|a| a.id);
        }
    }

    /// The session is over: nothing more can be sent.
    pub fn ended(&self) -> bool {
        matches!(self.conn, ConnState::Ended(_)) || self.status.is_some_and(|s| s.is_terminal())
    }

    /// A turn is in progress. The worker's status when it has reported one — so a
    /// reconnect in a quiet stretch of a turn still shows it, and a steer or the gap
    /// between queued turns doesn't flicker it off — else the event-derived guess.
    pub fn busy(&self) -> bool {
        if self.ended() {
            return false;
        }
        match self.status {
            Some(s) => !matches!(s, SessionStatus::Idle | SessionStatus::Starting),
            None => self.running,
        }
    }

    /// The open command a chunk of output belongs to: the newest one without an
    /// exit code. Not simply the last block — a notice (a blocked egress, sandbox
    /// bring-up) can land mid-command, and the output after it must not be lost.
    fn open_command(&mut self) -> Option<(&mut String, &mut String, &mut Option<i32>)> {
        self.blocks.iter_mut().rev().find_map(|b| match b {
            Block::Command {
                output, live, exit, ..
            } if exit.is_none() => Some((output, live, exit)),
            _ => None,
        })
    }

    /// The socket dropped (not a session end) — show "reconnecting" unless the
    /// session is already terminally ended.
    pub fn set_reconnecting(&mut self) {
        if !matches!(self.conn, ConnState::Ended(_)) {
            self.conn = ConnState::Reconnecting;
        }
    }

    pub fn set_unavailable(&mut self) {
        if !matches!(self.conn, ConnState::Ended(_)) {
            self.conn = ConnState::Unavailable;
        }
    }

    /// A fresh socket is open again.
    pub fn set_live(&mut self) {
        if !matches!(self.conn, ConnState::Ended(_)) {
            self.conn = ConnState::Live;
        }
    }

    fn apply_event(&mut self, ev: UiEventMsg) {
        match ev {
            // No optimistic echo: the journaled message *is* the echo, so live, replay
            // and other clients' messages all render identically, once each.
            UiEventMsg::UserMessage(m) => {
                self.running = true;
                self.commit();
                self.blocks.push(Block::User(m));
            }
            UiEventMsg::Delta(t) => {
                self.running = true;
                self.streaming.push_str(&t);
            }
            UiEventMsg::Reasoning(t) => {
                self.running = true;
                self.reasoning.push_str(&t);
            }
            UiEventMsg::ModelDone => self.commit(),
            UiEventMsg::CommandStart(cmd) => {
                self.commit();
                self.running = true;
                self.blocks.push(Block::Command {
                    cmd,
                    output: String::new(),
                    live: String::new(),
                    exit: None,
                });
            }
            UiEventMsg::CommandOutput(chunk) => {
                if let Some((output, live, _)) = self.open_command() {
                    // A chunk without a newline is the current line's latest state
                    // (a `\r` progress update); it replaces the previous one.
                    if chunk.ends_with('\n') {
                        live.clear();
                        output.push_str(&chunk);
                    } else {
                        *live = chunk;
                    }
                }
            }
            // `output` is ignored, as the TUI ignores it: the text already streamed
            // as `CommandOutput`, and appending it again would duplicate it.
            UiEventMsg::CommandEnd { code, .. } => {
                if let Some((output, live, exit)) = self.open_command() {
                    output.push_str(&std::mem::take(live));
                    *exit = Some(code);
                }
            }
            UiEventMsg::ToolUse(s) => {
                self.commit();
                self.blocks.push(Block::Tool(s));
            }
            UiEventMsg::FileDiff { path, diff } => {
                self.commit();
                self.blocks.push(Block::Diff { path, diff });
            }
            UiEventMsg::Final(m) => {
                self.commit();
                // The loop emits `Final` with the whole answer, which usually
                // repeats the just-committed model output. Re-tag that last block
                // as the final rather than rendering it twice (mirrors the TUI).
                match self.blocks.last_mut() {
                    Some(Block::Agent(prev)) if prev.trim() == m.trim() => {
                        *self.blocks.last_mut().unwrap() = Block::Final(m);
                    }
                    _ => self.blocks.push(Block::Final(m)),
                }
            }
            UiEventMsg::Notice(m) => self.blocks.push(Block::Notice(m)),
            UiEventMsg::DiffStat(s) => self.diffstat = s,
            UiEventMsg::Tokens { input, output } => {
                self.tokens_in = input;
                self.tokens_out = output;
            }
            UiEventMsg::Cost(c) => self.cost_usd = c,
            UiEventMsg::ContextUsage {
                used,
                budget,
                window,
                reserve,
                top,
            } => {
                self.context = Some(Context {
                    used,
                    budget,
                    window,
                    reserve,
                    top,
                });
            }
            // Banner for the current state, plus a transcript line for each change
            // (as the TUI does), so a past block and its reason aren't lost.
            UiEventMsg::Blocked(r) => {
                if r != self.blocked {
                    self.push_notice(match &r {
                        Some(reason) => format!("⏸ blocked: {reason}"),
                        None => "▶ unblocked".into(),
                    });
                }
                self.blocked = r;
            }
            UiEventMsg::Plan(p) => self.plan = p,
            UiEventMsg::Title(t) => self.title = t,
            // Flush what the turn streamed: an interrupted model call ends without
            // `ModelDone`, and its partial answer must not stay looking live.
            UiEventMsg::TurnDone => {
                self.commit();
                self.running = false;
            }
            UiEventMsg::SubagentPending { label, model, id } => {
                // A fresh fan-out (none still pending or running) replaces the batch.
                if !self.subagents.iter().any(|s| s.done.is_none()) {
                    self.subagents.clear();
                }
                self.subagents.push(SubagentStatus {
                    id,
                    label,
                    model,
                    pending: true,
                    done: None,
                    requested: 0,
                    used: 0,
                    granted: 0,
                    asking: false,
                    elapsed_ms: 0,
                    task: String::new(),
                });
            }
            UiEventMsg::SubagentStarted { label, model, id } => {
                // Flip an existing pending entry to running; otherwise it's a fresh
                // start (throttle disabled, or an older worker with no pending event).
                if let Some(s) = self
                    .subagents
                    .iter_mut()
                    .find(|s| s.id == id && s.pending && s.done.is_none())
                {
                    s.pending = false;
                } else {
                    if !self.subagents.iter().any(|s| s.done.is_none()) {
                        self.subagents.clear();
                    }
                    self.subagents.push(SubagentStatus {
                        id,
                        label,
                        model,
                        pending: false,
                        done: None,
                        requested: 0,
                        used: 0,
                        granted: 0,
                        asking: false,
                        elapsed_ms: 0,
                        task: String::new(),
                    });
                }
            }
            UiEventMsg::SubagentDone { ok, id, .. } => {
                if let Some(s) = self
                    .subagents
                    .iter_mut()
                    .find(|s| s.id == id && s.done.is_none())
                {
                    s.done = Some(ok);
                }
            }
            // Level-triggered job state: authoritative where it is available, since the
            // `Subagent*` edges cannot express a worker parked waiting for a decision or
            // its turn usage.
            UiEventMsg::JobsChanged(jobs) => {
                self.subagents = jobs
                    .into_iter()
                    .map(|j| SubagentStatus {
                        pending: j.state == "pending",
                        done: match j.state.as_str() {
                            "done" => Some(true),
                            "failed" => Some(false),
                            _ => None,
                        },
                        requested: if j.state == "awaiting verdict" {
                            j.requested
                        } else {
                            0
                        },
                        used: j.used,
                        granted: j.granted,
                        asking: j.state == "asking a question",
                        elapsed_ms: j.elapsed_ms,
                        task: j.task,
                        id: j.id,
                        label: j.label,
                        model: j.model,
                    })
                    .collect();
            }
            UiEventMsg::QueueChanged { pending } => self.queued = pending,
            // Shown in the transcript, so a message that steered the turn is not
            // indistinguishable from one that started it.
            UiEventMsg::SteerDelivered(text) => self
                .blocks
                .push(Block::Notice(format!("↳ steering: {text}"))),
            UiEventMsg::NetEvent(e) => {
                self.net.push(e);
                if self.net.len() > NET_EVENTS_KEPT {
                    let drop = self.net.len() - NET_EVENTS_KEPT;
                    self.net.drain(..drop);
                }
            }
            UiEventMsg::Processes(p) => self.processes = p,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: &str, label: &str) -> UiEventMsg {
        UiEventMsg::SubagentStarted {
            label: label.into(),
            model: "m".into(),
            id: id.into(),
        }
    }

    /// A job snapshot replaces the chip row and carries the states the edge events
    /// cannot express.
    ///
    /// The reason this test exists: this crate is not a workspace member, so a new
    /// `UiEventMsg` variant breaks it invisibly to `cargo build --workspace`. A test
    /// that names the variant fails loudly instead.
    #[test]
    fn a_job_snapshot_drives_the_chips_including_a_pending_turn_request() {
        use cowboy_proto::daemonproto::JobInfo;
        let job = |id: &str, state: &str, requested: u32| JobInfo {
            id: id.into(),
            label: format!("l-{id}"),
            model: "m".into(),
            task: "t".into(),
            state: state.into(),
            elapsed_ms: 1000,
            used: 25,
            granted: 25,
            ceiling: 400,
            requested,
        };
        let mut m = Model::default();
        m.apply_event(UiEventMsg::JobsChanged(vec![
            job("a", "running", 0),
            job("b", "awaiting verdict", 30),
            job("c", "pending", 0),
            job("d", "done", 0),
            job("e", "failed", 0),
        ]));
        assert_eq!(m.subagents.len(), 5);
        let by = |id: &str| m.subagents.iter().find(|s| s.id == id).unwrap().clone();
        assert_eq!(by("a").done, None);
        assert_eq!(by("a").requested, 0);
        // Waiting for a verdict is not "done" and not "pending": it is its own state.
        assert_eq!(by("b").done, None);
        assert!(!by("b").pending);
        assert_eq!(by("b").requested, 30);
        assert!(by("c").pending);
        assert_eq!(by("d").done, Some(true));
        assert_eq!(by("e").done, Some(false));

        // A later snapshot replaces the row rather than accumulating stale chips.
        m.apply_event(UiEventMsg::JobsChanged(vec![job("a", "done", 0)]));
        assert_eq!(m.subagents.len(), 1);
    }

    #[test]
    fn the_queue_and_steering_reach_the_view() {
        let mut m = Model::default();
        m.apply_event(UiEventMsg::QueueChanged {
            pending: vec!["then update the docs".into()],
        });
        assert_eq!(m.queued, vec!["then update the docs".to_string()]);
        m.apply_event(UiEventMsg::SteerDelivered("check the error path".into()));
        assert!(m.blocks.iter().any(|b| matches!(
            b,
            Block::Notice(n) if n.contains("check the error path")
        )));
    }

    /// Context utilisation reaches the header.
    ///
    /// This variant was added to the wire protocol for the TUI's `/context` view and
    /// the web UI's match was not updated — which failed no build, because
    /// `cowboy-web-ui` is wasm32-only and not a workspace member, so `cargo build
    /// --workspace` never compiles it, and the stale bundle in `dist/` kept embedding
    /// happily. Matching every variant explicitly (no `_ =>` arm) is what makes the
    /// compiler catch the next one; this keeps that honest.
    #[test]
    fn context_usage_lands_in_the_header_stats() {
        let mut m = Model::default();
        assert!(m.context.is_none(), "nothing to show before the first turn");
        m.apply_event(UiEventMsg::ContextUsage {
            used: 84_500,
            budget: 160_000,
            window: 200_000,
            reserve: 40_000,
            top: vec![("shell".into(), 30_000)],
        });
        let c = m.context.as_ref().unwrap();
        assert_eq!(
            (c.used, c.budget, c.window, c.reserve),
            (84_500, 160_000, 200_000, 40_000)
        );
        assert_eq!(c.top[0].0, "shell");
        assert_eq!(c.percent(), 52);
    }

    #[test]
    fn tracks_subagents_and_freezes_on_end() {
        let mut m = Model::default();
        m.apply_event(started("a", "arch"));
        m.apply_event(started("b", "tests"));
        assert_eq!(m.subagents.len(), 2);

        m.apply_event(UiEventMsg::SubagentDone {
            label: "tests".into(),
            ok: true,
            id: "b".into(),
        });
        assert_eq!(
            m.subagents.iter().find(|s| s.id == "b").unwrap().done,
            Some(true)
        );
        assert_eq!(m.subagents.iter().find(|s| s.id == "a").unwrap().done, None);

        // Session end freezes any still-running subagent (no forever "running").
        m.apply(ServerMsg::Ended { reason: "x".into() });
        assert_eq!(
            m.subagents.iter().find(|s| s.id == "a").unwrap().done,
            Some(false)
        );
    }

    #[test]
    fn ask_resolution_only_clears_the_matching_prompt() {
        let mut m = Model::default();
        m.apply(ServerMsg::Ask {
            id: 7,
            question: "continue?".into(),
            options: Vec::new(),
            choices: Vec::new(),
        });
        m.apply(ServerMsg::AskResolved { id: 6 });
        assert_eq!(m.asks.first().map(|ask| ask.id), Some(7));
        m.apply(ServerMsg::AskResolved { id: 7 });
        assert!(m.asks.is_empty());
    }

    #[test]
    fn snapshot_prompts_are_authoritative() {
        let mut m = Model::default();
        m.apply(ServerMsg::Ask {
            id: 1,
            question: "stale?".into(),
            options: Vec::new(),
            choices: Vec::new(),
        });
        let snapshot = |pending_prompts| ServerMsg::Snapshot {
            info: serde_json::from_value(serde_json::json!({
                "id": "s1",
                "root": "/tmp/p",
                "status": "running",
            }))
            .unwrap(),
            journal_len: 0,
            pending_prompts,
        };
        m.apply(snapshot(vec![
            PendingPrompt::Ask {
                id: 2,
                question: "continue?".into(),
                options: vec!["yes".into()],
                choices: Vec::new(),
            },
            PendingPrompt::Ask {
                id: 3,
                question: "later".into(),
                options: Vec::new(),
                choices: Vec::new(),
            },
            PendingPrompt::Approval {
                id: 4,
                dest: "example.com:443".into(),
                detail: None,
            },
        ]));
        assert_eq!(m.asks.first().map(|a| a.id), Some(2));
        assert_eq!(m.approvals.first().map(|a| a.id), Some(4));
        m.apply(snapshot(Vec::new()));
        assert!(m.asks.is_empty() && m.approvals.is_empty());
    }

    /// Two approvals outstanding: the older is shown first, and answering the newer
    /// must not lose it (it used to overwrite a single slot, then clear it).
    #[test]
    fn concurrent_approvals_queue_oldest_first() {
        let mut m = Model::default();
        let ap = |id| ServerMsg::Approval {
            id,
            dest: format!("h{id}:443"),
            detail: None,
        };
        m.apply(ap(5));
        m.apply(ap(4));
        assert_eq!(m.approvals.first().map(|a| a.id), Some(4));
        m.apply(ServerMsg::ApprovalResolved { id: 5 });
        assert_eq!(
            m.approvals.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![4]
        );
    }

    /// Output after a mid-command notice still reaches the command, and so does its
    /// exit code — a failed `curl` whose egress was blocked must not look clean.
    #[test]
    fn command_output_survives_a_notice_and_progress_lines_overwrite() {
        let mut m = Model::default();
        m.apply_event(UiEventMsg::CommandStart("curl x".into()));
        m.apply_event(UiEventMsg::CommandOutput("start\n".into()));
        m.apply_event(UiEventMsg::Notice("🛡 blocked x:443".into()));
        m.apply_event(UiEventMsg::CommandOutput("10%".into()));
        m.apply_event(UiEventMsg::CommandOutput("50%".into()));
        m.apply_event(UiEventMsg::CommandOutput("100%\n".into()));
        m.apply_event(UiEventMsg::CommandEnd {
            code: 7,
            output: String::new(),
        });
        let Some(Block::Command {
            output, live, exit, ..
        }) = m.blocks.first()
        else {
            panic!("command block first");
        };
        assert_eq!(output, "start\n100%\n");
        assert!(live.is_empty());
        assert_eq!(*exit, Some(7));
    }

    /// An interrupted turn ends without `ModelDone`; its partial text is committed.
    #[test]
    fn turn_done_commits_a_partial_answer() {
        let mut m = Model::default();
        m.apply_event(UiEventMsg::Reasoning("hmm".into()));
        m.apply_event(UiEventMsg::Delta("half an ans".into()));
        m.apply_event(UiEventMsg::TurnDone);
        assert!(m.streaming.is_empty() && m.reasoning.is_empty());
        assert!(matches!(m.blocks.last(), Some(Block::Agent(t)) if t == "half an ans"));
    }

    /// The journal is the only source of user messages, so a replay renders two
    /// identical consecutive messages twice, as they were sent.
    #[test]
    fn repeated_user_messages_are_not_collapsed() {
        let mut m = Model::default();
        m.apply_event(UiEventMsg::UserMessage("again".into()));
        m.apply_event(UiEventMsg::UserMessage("again".into()));
        assert_eq!(
            m.blocks
                .iter()
                .filter(|b| matches!(b, Block::User(_)))
                .count(),
            2
        );
    }

    /// Busy comes from the worker's status when it has one, so it survives the
    /// gap between queued turns and a reconnect mid-turn.
    #[test]
    fn busy_follows_the_worker_status_and_stops_at_end() {
        let mut m = Model::default();
        m.apply(ServerMsg::Status(SessionStatus::Running));
        m.apply_event(UiEventMsg::TurnDone);
        assert!(
            m.busy(),
            "a queued turn follows; the worker still says running"
        );
        m.apply(ServerMsg::Status(SessionStatus::Idle));
        assert!(!m.busy());
        m.apply(ServerMsg::Ask {
            id: 1,
            question: "q".into(),
            options: vec![],
            choices: Vec::new(),
        });
        m.apply(ServerMsg::Ended {
            reason: "done".into(),
        });
        assert!(!m.busy() && m.ended() && m.asks.is_empty());
    }

    #[test]
    fn blocked_changes_leave_a_transcript_line() {
        let mut m = Model::default();
        m.apply_event(UiEventMsg::Blocked(Some("need creds".into())));
        m.apply_event(UiEventMsg::Blocked(None));
        let notices: Vec<_> = m
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Notice(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(notices, ["⏸ blocked: need creds", "▶ unblocked"]);
    }

    #[test]
    fn fresh_fanout_replaces_previous_batch() {
        let mut m = Model::default();
        m.apply_event(started("a", "one"));
        m.apply_event(UiEventMsg::SubagentDone {
            label: "one".into(),
            ok: true,
            id: "a".into(),
        });
        // None running now → the next start replaces the batch.
        m.apply_event(started("b", "two"));
        assert_eq!(m.subagents.len(), 1);
        assert_eq!(m.subagents[0].id, "b");
    }
}
