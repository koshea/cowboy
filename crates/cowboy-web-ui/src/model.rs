//! The browser-side transcript model: the same shape as the TUI's `App` state,
//! mutated by the wire `ServerMsg`/`UiEventMsg` stream. Kept free of Yew/web-sys
//! so it stays pure and mirrors `apply_wire` 1:1.

use cowboy_proto::daemonproto::{ServerMsg, SessionStatus, UiEventMsg};

/// One rendered transcript entry.
#[derive(Clone, PartialEq)]
pub enum Block {
    User(String),
    Agent(String),
    Command {
        cmd: String,
        output: String,
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
    /// The session itself ended (terminal — no reconnect).
    Ended(String),
}

/// A pending question or approval awaiting the user.
#[derive(Clone, PartialEq)]
pub struct Ask {
    pub id: u64,
    pub question: String,
    pub options: Vec<String>,
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
}

#[derive(Clone, PartialEq, Default)]
pub struct Model {
    pub title: String,
    pub blocks: Vec<Block>,
    /// In-progress (un-committed) model output.
    pub streaming: String,
    /// Streamed reasoning, shown dimmed until the turn commits.
    pub reasoning: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    /// Conversation tokens against the conversation budget, from the loop's own
    /// accounting — the same numbers `/context` shows in the TUI. `None` until the
    /// first turn reports them.
    ///
    /// Only the two figures the header needs: the window/reserve split and the list of
    /// top consumers are a diagnostic view the phone-sized header has no room for.
    pub context: Option<(u64, u64)>,
    pub diffstat: String,
    pub plan: Vec<(String, String)>,
    pub blocked: Option<String>,
    pub ask: Option<Ask>,
    pub approval: Option<Approval>,
    pub status: Option<SessionStatus>,
    pub conn: ConnState,
    /// A turn is in flight (drives the spinner / disables nothing).
    pub running: bool,
    /// Crew subagents dispatched this session, for the watch list.
    pub subagents: Vec<SubagentStatus>,
    /// Input the user queued to run after the current turn.
    pub queued: Vec<String>,
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
            ServerMsg::Snapshot { info, .. } => {
                self.conn = ConnState::Live;
                self.status = Some(info.status);
                if self.title.is_empty() {
                    self.title = info.task.unwrap_or(info.id);
                }
            }
            ServerMsg::Event { event, .. } => self.apply_event(event),
            ServerMsg::Ask {
                id,
                question,
                options,
            } => {
                self.ask = Some(Ask {
                    id,
                    question,
                    options,
                });
            }
            ServerMsg::Approval { id, dest, detail } => {
                use cowboy_proto::netproto::ApprovalKind;
                let (title, rows, note) = match detail {
                    Some(d) => (
                        match d.kind {
                            ApprovalKind::Network => "Network request",
                            ApprovalKind::Credential => "Credential access",
                        },
                        d.rows,
                        d.note,
                    ),
                    None => ("Approval", Vec::new(), None),
                };
                self.approval = Some(Approval {
                    id,
                    dest,
                    title: title.to_string(),
                    rows,
                    note,
                });
            }
            ServerMsg::ApprovalResolved { id } => {
                if self.approval.as_ref().is_some_and(|a| a.id == id) {
                    self.approval = None;
                }
            }
            ServerMsg::Status(s) => self.status = Some(s),
            ServerMsg::Ended { reason } => {
                self.commit();
                self.running = false;
                self.conn = ConnState::Ended(reason);
                // The worker may die before reporting in-flight subagents done;
                // freeze them so the list doesn't show them forever "running".
                for s in &mut self.subagents {
                    if s.done.is_none() {
                        s.done = Some(false);
                    }
                }
            }
        }
    }

    /// Optimistically echo a message the user just sent, so it appears instantly
    /// (the worker's journaled `UserMessage` echo is then deduped in `apply`).
    pub fn push_user(&mut self, text: String) {
        self.commit();
        self.blocks.push(Block::User(text));
        self.running = true;
    }

    /// The socket dropped (not a session end) — show "reconnecting" unless the
    /// session is already terminally ended.
    pub fn set_reconnecting(&mut self) {
        if !matches!(self.conn, ConnState::Ended(_)) {
            self.conn = ConnState::Reconnecting;
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
            UiEventMsg::UserMessage(m) => {
                self.running = true;
                // Skip the journaled echo of our own optimistic local push (live
                // send); render genuinely-new ones — a journal replay on refresh,
                // or a message another client sent.
                if !matches!(self.blocks.last(), Some(Block::User(prev)) if *prev == m) {
                    self.commit();
                    self.blocks.push(Block::User(m));
                }
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
                    exit: None,
                });
            }
            UiEventMsg::CommandOutput(chunk) => {
                if let Some(Block::Command { output, .. }) = self.blocks.last_mut() {
                    output.push_str(&chunk);
                }
            }
            UiEventMsg::CommandEnd { code, .. } => {
                if let Some(Block::Command { exit, .. }) = self.blocks.last_mut() {
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
            UiEventMsg::ContextUsage { used, budget, .. } => {
                self.context = Some((used, budget));
            }
            UiEventMsg::Blocked(r) => self.blocked = r,
            UiEventMsg::Plan(p) => self.plan = p,
            UiEventMsg::Title(t) => self.title = t,
            UiEventMsg::TurnDone => self.running = false,
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
            // Process / net-activity panes are not rendered in v1.
            UiEventMsg::NetEvent(_) | UiEventMsg::Processes(_) => {}
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
        assert_eq!(m.context, None, "nothing to show before the first turn");
        m.apply_event(UiEventMsg::ContextUsage {
            used: 84_500,
            budget: 160_000,
            window: 200_000,
            reserve: 40_000,
            top: vec![("shell".into(), 30_000)],
        });
        assert_eq!(m.context, Some((84_500, 160_000)));
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
