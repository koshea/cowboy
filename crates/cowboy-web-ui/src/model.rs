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
    pub ended: Option<String>,
    /// A turn is in flight (drives the spinner / disables nothing).
    pub running: bool,
    /// True once the initial journal snapshot has been received.
    pub connected: bool,
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
                self.connected = true;
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
                self.ended = Some(reason);
            }
        }
    }

    fn apply_event(&mut self, ev: UiEventMsg) {
        match ev {
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
                self.blocks.push(Block::Final(m));
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

    /// A user message we send locally — echoed into the transcript immediately so
    /// the sender sees it without waiting for a round-trip.
    pub fn push_user(&mut self, text: String) {
        self.blocks.push(Block::User(text));
        self.running = true;
    }
}
