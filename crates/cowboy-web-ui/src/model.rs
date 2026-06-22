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
    pub diffstat: String,
    pub plan: Vec<(String, String)>,
    pub blocked: Option<String>,
    pub ask: Option<Ask>,
    pub approval: Option<Approval>,
    pub status: Option<SessionStatus>,
    pub conn: ConnState,
    /// A turn is in flight (drives the spinner / disables nothing).
    pub running: bool,
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
            ServerMsg::Approval { id, dest } => {
                self.approval = Some(Approval { id, dest });
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
            UiEventMsg::Blocked(r) => self.blocked = r,
            UiEventMsg::Plan(p) => self.plan = p,
            UiEventMsg::Title(t) => self.title = t,
            UiEventMsg::TurnDone => self.running = false,
            // Crew / process / net-activity panes are not rendered in v1.
            UiEventMsg::NetEvent(_)
            | UiEventMsg::Processes(_)
            | UiEventMsg::SubagentStarted { .. }
            | UiEventMsg::SubagentDone { .. } => {}
        }
    }
}
