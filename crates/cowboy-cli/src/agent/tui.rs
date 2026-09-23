//! ratatui front-end: a [`TuiUi`] adapter implementing [`AgentUi`] plus the
//! terminal event loop.
//!
//! Threading model: the async agent loop runs on a dedicated thread with its
//! own current-thread runtime and holds the `TuiUi`, which forwards display
//! events to the main thread over a channel. `ask_user` blocks the agent
//! thread on a reply channel — safe because it is not a runtime worker shared
//! with anything else.

use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use cowboy_core::daemonproto::{SessionStatus, UiEventMsg};
use cowboy_core::netproto::{ApprovalDetail, ApprovalKind, ApprovalScope, Verdict};
use cowboy_tui::{
    draw, Access, App, ConnectionState, CrewMember, CrewStatus, LineKind, Mode, ModelChoice,
    ModelForm, ModelPicker, REASONING_OPTS,
};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use super::help;
use super::ui::AgentUi;

/// A command the TUI sends to the agent thread.
pub enum AgentCmd {
    /// A user message to run as a turn.
    Message(String),
    /// A user message to run *after* the current turn, rather than steering it.
    Enqueue(String),
    /// Drop everything queued for after the current turn.
    QueueClear,
    /// Switch the active model to this name (applies from the next turn).
    SwitchModel(String),
    /// Turn plan mode on/off (file edits are blocked while on).
    PlanMode(bool),
    /// Sign off on this session's ranch workstream (the user typed `/accept`):
    /// complete the workstream, advance the plan, and end the session.
    Accept { note: Option<String> },
    /// Stop the background subagents, leaving the session and the current turn alone.
    StopSubagents,
    /// A session slash command for the worker to expand (`agent::commands`), so
    /// the TUI and the web run it identically. Without the leading `/`.
    Command(String),
    /// Detach this client, leaving the session running for later re-attach.
    Detach,
    /// End the session.
    ///
    /// Explicit rather than inferred from dropping the sender. The channel-hangup
    /// route still exists as a backstop for a client that dies without asking, but
    /// "the user pressed end" deserves a message of its own: a side effect of a drop
    /// is invisible in a log, untestable in isolation, and was the prime suspect when
    /// ending a session left the worker running.
    End,
}

/// Events the agent loop / control server send to the TUI event loop.
///
/// Most events are the journaled display events shared with the daemon wire
/// protocol — they ride inside [`UiEvent::Wire`] rather than being restated
/// here, so the two enums can't drift. The remaining variants are client-only:
/// they carry non-serializable reply channels or are synthesized by the client.
/// One reply-capable prompt delivered to the terminal. The descriptor and reply
/// channel stay together so authoritative snapshots can reconcile by ID without
/// ever manufacturing an empty answer when an obsolete channel is dropped.
#[derive(Debug)]
pub enum UiPrompt {
    Ask {
        id: u64,
        question: String,
        options: Vec<String>,
        reply: Sender<String>,
    },
    Approval {
        id: u64,
        dest: String,
        detail: Option<ApprovalDetail>,
        reply: tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>,
    },
}

impl UiPrompt {
    fn id(&self) -> u64 {
        match self {
            Self::Ask { id, .. } | Self::Approval { id, .. } => *id,
        }
    }

    fn same_kind(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Ask { .. }, Self::Ask { .. }) | (Self::Approval { .. }, Self::Approval { .. })
        )
    }
}

#[derive(Debug)]
pub enum UiEvent {
    /// A journaled display event (the shared [`UiEventMsg`] payload).
    Wire(UiEventMsg),
    /// A question for the user: id, prompt, suggested options, and reply channel.
    Ask(u64, String, Vec<String>, Sender<String>),
    /// An approval request: id, display state, and reply channel.
    Approval(
        u64,
        String,
        Option<ApprovalDetail>,
        tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>,
    ),
    /// Replace all reply-capable prompts from an authoritative worker snapshot.
    ReplacePrompts(Vec<UiPrompt>),
    AskResolved(u64),
    ApprovalResolved(u64),
    Connection(ConnectionState),
    Lifecycle(SessionStatus),
    /// The `/models` catalogue finished loading; open the picker.
    ModelsFetched(Vec<ModelChoice>),
    /// The session ended.
    Done,
}

#[derive(Default)]
struct PendingPrompts {
    items: Vec<UiPrompt>,
    active: Option<u64>,
    return_mode: Option<Mode>,
}

impl PendingPrompts {
    fn add(&mut self, app: &mut App, prompt: UiPrompt) {
        if app.access.is_read_only() {
            return;
        }
        let id = prompt.id();
        if let Some(existing) = self.items.iter().position(|old| old.id() == id) {
            if self.items[existing].same_kind(&prompt) {
                // A snapshot/live overlap may describe the same prompt twice. Keep
                // the channel already displayed by the TUI; dropping the newcomer
                // only closes a forwarder that sends nothing on cancellation.
                return;
            }
            self.items.remove(existing);
            if self.active == Some(id) {
                self.active = None;
            }
        }
        self.items.push(prompt);
        self.items.sort_by_key(UiPrompt::id);
        self.reconcile(app);
    }

    fn replace(&mut self, app: &mut App, incoming: Vec<UiPrompt>) {
        if app.access.is_read_only() {
            self.items.clear();
            self.active = None;
            return;
        }
        let mut old = std::mem::take(&mut self.items);
        self.items = incoming
            .into_iter()
            .map(|prompt| {
                let same = old.iter().position(|candidate| {
                    candidate.id() == prompt.id() && candidate.same_kind(&prompt)
                });
                same.map(|index| old.remove(index)).unwrap_or(prompt)
            })
            .collect();
        self.items.sort_by_key(UiPrompt::id);
        if self
            .active
            .is_some_and(|id| !self.items.iter().any(|prompt| prompt.id() == id))
        {
            self.active = None;
            Self::clear_modal(app);
        }
        self.reconcile(app);
    }

    fn resolve(&mut self, app: &mut App, id: u64) {
        let was_active = self.active == Some(id);
        self.items.retain(|prompt| prompt.id() != id);
        if was_active {
            self.active = None;
            Self::clear_modal(app);
        }
        self.reconcile(app);
    }

    fn answer_ask(&mut self, app: &mut App, answer: String) {
        let Some(id) = self.active else { return };
        let Some(index) = self.items.iter().position(|prompt| prompt.id() == id) else {
            return;
        };
        let prompt = self.items.remove(index);
        self.active = None;
        Self::clear_modal(app);
        if let UiPrompt::Ask { reply, .. } = prompt {
            let _ = reply.send(answer);
        }
        self.reconcile(app);
    }

    fn answer_approval(&mut self, app: &mut App, answer: (Verdict, ApprovalScope)) {
        let Some(id) = self.active else { return };
        let Some(index) = self.items.iter().position(|prompt| prompt.id() == id) else {
            return;
        };
        let prompt = self.items.remove(index);
        self.active = None;
        Self::clear_modal(app);
        if let UiPrompt::Approval { reply, .. } = prompt {
            let _ = reply.send(answer);
        }
        self.reconcile(app);
    }

    fn reconcile(&mut self, app: &mut App) {
        if app.access.is_read_only() || self.active.is_some() {
            return;
        }
        let Some(prompt) = self.items.first() else {
            if let Some(mode) = self.return_mode.take() {
                app.mode = mode;
            }
            return;
        };
        self.return_mode.get_or_insert_with(|| app.mode.clone());
        self.active = Some(prompt.id());
        app.commit_stream();
        match prompt {
            UiPrompt::Ask {
                question, options, ..
            } if options.is_empty() => {
                app.mode = Mode::AwaitingInput(question.clone());
            }
            UiPrompt::Ask {
                question, options, ..
            } => app.begin_choice(question.clone(), options.clone()),
            UiPrompt::Approval { dest, detail, .. } => {
                app.begin_approval(dest.clone(), detail.clone().map(approval_view));
            }
        }
    }

    fn clear_modal(app: &mut App) {
        app.choice = None;
        app.end_approval();
    }
}

/// `AgentUi` implementation that forwards to the TUI thread.
pub struct TuiUi {
    pub tx: Sender<UiEvent>,
}

impl TuiUi {
    fn wire(&self, e: UiEventMsg) {
        let _ = self.tx.send(UiEvent::Wire(e));
    }
}

impl AgentUi for TuiUi {
    fn model_delta(&mut self, text: &str) {
        self.wire(UiEventMsg::Delta(text.to_string()));
    }
    fn model_reasoning(&mut self, text: &str) {
        self.wire(UiEventMsg::Reasoning(text.to_string()));
    }
    fn model_done(&mut self) {
        self.wire(UiEventMsg::ModelDone);
    }
    fn command_start(&mut self, command: &str) {
        self.wire(UiEventMsg::CommandStart(command.to_string()));
    }
    fn command_output(&mut self, chunk: &str) {
        self.wire(UiEventMsg::CommandOutput(chunk.to_string()));
    }
    fn command_end(&mut self, exit_code: i32, output: &str) {
        self.wire(UiEventMsg::CommandEnd {
            code: exit_code,
            output: output.to_string(),
        });
    }
    fn tool_use(&mut self, summary: &str) {
        self.wire(UiEventMsg::ToolUse(summary.to_string()));
    }
    fn file_diff(&mut self, path: &str, diff: &str) {
        self.wire(UiEventMsg::FileDiff {
            path: path.to_string(),
            diff: diff.to_string(),
        });
    }
    fn tokens(&mut self, input: u64, output: u64) {
        self.wire(UiEventMsg::Tokens { input, output });
    }
    fn context_usage(&mut self, u: &crate::agent::ui::ContextUsage) {
        self.wire(UiEventMsg::ContextUsage {
            used: u.used,
            budget: u.budget,
            window: u.window,
            reserve: u.reserve,
            top: u.top.clone(),
        });
    }
    fn cost(&mut self, usd: f64) {
        self.wire(UiEventMsg::Cost(usd));
    }
    fn plan(&mut self, steps: &[(String, String)]) {
        self.wire(UiEventMsg::Plan(steps.to_vec()));
    }
    fn blocked(&mut self, reason: Option<&str>) {
        self.wire(UiEventMsg::Blocked(reason.map(str::to_string)));
    }
    fn final_message(&mut self, message: &str) {
        self.wire(UiEventMsg::Final(message.to_string()));
    }
    fn ask_user(&mut self, question: &str, options: &[String]) -> String {
        let (rtx, rrx) = std::sync::mpsc::channel();
        if self
            .tx
            .send(UiEvent::Ask(0, question.to_string(), options.to_vec(), rtx))
            .is_err()
        {
            return String::new();
        }
        rrx.recv().unwrap_or_default()
    }
    fn notice(&mut self, msg: &str) {
        self.wire(UiEventMsg::Notice(msg.to_string()));
    }
}

/// Apply a journaled (wire) display event to the view state. Pure view-state
/// mutation; control-flow events (Ask/Approval/TurnDone/Done) stay in the loop.
fn apply_wire(app: &mut App, msg: UiEventMsg) {
    match msg {
        // Interactive submission is echoed locally before it reaches the worker.
        // Observers and replay have no local echo, so the journal is their only
        // source for the user's side of the conversation.
        UiEventMsg::UserMessage(message) => {
            if app.access.is_read_only() {
                app.push(LineKind::User, message);
            }
        }
        UiEventMsg::Delta(t) => app.stream(&t),
        UiEventMsg::Reasoning(t) => app.stream_reasoning(&t),
        UiEventMsg::ModelDone => app.commit_stream(),
        UiEventMsg::CommandStart(c) => {
            app.commit_stream();
            app.push(LineKind::Command, c.clone());
            app.start_command(c, now_ms());
        }
        UiEventMsg::CommandOutput(chunk) => {
            // A committed line carries a trailing newline; a transient
            // (carriage-return progress) update doesn't — it overwrites the
            // previous line in place.
            let committed = chunk.ends_with('\n');
            app.command_output_line(chunk.trim_end_matches('\n'), committed);
        }
        UiEventMsg::CommandEnd { code, .. } => {
            if code != 0 {
                app.push(LineKind::Error, format!("[exit {code}]"));
            }
            app.end_command();
            app.status = "running".into();
        }
        UiEventMsg::ToolUse(s) => {
            app.commit_stream();
            app.push(LineKind::Tool, s);
        }
        UiEventMsg::FileDiff { diff, .. } => {
            app.commit_stream();
            app.push(LineKind::Diff, diff);
        }
        UiEventMsg::Final(m) => {
            // `final` ends the turn, not the session. ModelDone may have already
            // committed an implicit final's streamed content as an Agent line;
            // `push_final` re-tags it instead of duplicating.
            app.commit_stream();
            app.push_final(m);
        }
        UiEventMsg::Notice(m) => app.push(LineKind::Notice, m),
        UiEventMsg::NetEvent(line) => app.activity(line),
        UiEventMsg::DiffStat(s) => app.diff = s,
        UiEventMsg::Tokens { input, output } => {
            app.tokens_in = input;
            app.tokens_out = output;
        }
        UiEventMsg::ContextUsage {
            used,
            budget,
            window,
            reserve,
            top,
        } => {
            app.context = Some(cowboy_tui::ContextSnapshot {
                used,
                budget,
                window,
                reserve,
                top,
            });
        }
        UiEventMsg::Cost(usd) => app.cost_usd = usd,
        UiEventMsg::Plan(steps) => app.plan = steps,
        UiEventMsg::Blocked(reason) => app.set_blocked(reason),
        UiEventMsg::Title(t) => app.title = t,
        UiEventMsg::Processes(procs) => app.processes = procs,
        UiEventMsg::SubagentPending { label, model, id } => {
            app.subagent_pending(label, model, id, now_ms())
        }
        UiEventMsg::SubagentStarted { label, model, id } => {
            app.subagent_started(label, model, id, now_ms())
        }
        UiEventMsg::SubagentDone { ok, id, .. } => app.subagent_done(&id, ok),
        // Level-triggered job state: reconciles the background pane in one go,
        // including the states the edge events cannot express (waiting for a verdict,
        // turn usage).
        UiEventMsg::JobsChanged(jobs) => {
            // The wire → view mapping lives here, not in `cowboy-tui`: that crate is
            // rendering only and deliberately knows nothing about the protocol.
            let now = now_ms();
            let members = jobs
                .into_iter()
                .map(|j| CrewMember {
                    started_ms: now.saturating_sub(j.elapsed_ms),
                    elapsed_secs: j.elapsed_ms / 1000,
                    status: crew_status(&j.state),
                    id: j.id,
                    label: j.label,
                    model: j.model,
                    used: j.used,
                    granted: j.granted,
                    ceiling: j.ceiling,
                    requested: j.requested,
                })
                .collect();
            app.apply_jobs(members);
        }
        UiEventMsg::QueueChanged { pending } => app.queued = pending,
        UiEventMsg::SteerDelivered(text) => app.push(LineKind::Notice, format!("↳ {text}")),
        // Handled in the event loop (needs loop-local turn bookkeeping).
        UiEventMsg::TurnDone => {}
    }
}

/// Messages this client sent (and echoed locally) whose journaled copy has not come
/// back yet. The worker journals every `Message`/`Enqueue` it receives as a
/// `UserMessage`, and so does every other client's input; this is what lets the TUI
/// render the second without doubling the first.
///
/// Recorded at the send (see [`TaskTx`]), so it holds the text actually sent — a slash
/// command echoes `/plan …` but sends a canned prompt, and it is the prompt that comes
/// back.
#[derive(Clone, Default)]
pub(crate) struct LocalEcho(std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<String>>>);

impl LocalEcho {
    /// Bound on outstanding echoes: a send the worker never journals (the connection
    /// dropped under it) must not grow this without limit.
    const CAP: usize = 64;

    fn record(&self, text: String) {
        let mut q = self.0.borrow_mut();
        if q.len() >= Self::CAP {
            q.pop_front();
        }
        q.push_back(text);
    }

    /// True (and consumed) when `text` is one of ours. The worker journals our sends in
    /// order, so anything queued *ahead* of the match was lost in transit and is dropped
    /// with it — otherwise one lost send would wedge the front forever and every later
    /// echo would render twice.
    fn take(&self, text: &str) -> bool {
        let mut q = self.0.borrow_mut();
        match q.iter().position(|sent| sent == text) {
            Some(i) => {
                q.drain(..=i);
                true
            }
            None => false,
        }
    }
}

/// The TUI's handle on the agent: a `Sender<AgentCmd>` that records every message it
/// sends in the [`LocalEcho`], so no send site can forget to.
pub(crate) struct TaskTx {
    tx: Sender<AgentCmd>,
    echo: LocalEcho,
}

impl TaskTx {
    fn send(&self, cmd: AgentCmd) -> Result<(), std::sync::mpsc::SendError<AgentCmd>> {
        if let AgentCmd::Message(m) | AgentCmd::Enqueue(m) = &cmd {
            self.echo.record(m.clone());
        }
        self.tx.send(cmd)
    }
}

/// Apply a journaled event on the event loop, where the turn state lives.
///
/// Running/idle follows the worker rather than a local count of messages sent: a
/// message sent mid-turn *steers* that turn (one `TurnDone`, not two), and a turn can
/// be started by another client entirely. So a journaled `UserMessage` means a turn is
/// (or is about to be) running, and `TurnDone` with nothing queued means it is not.
fn apply_live_wire(app: &mut App, prompts: &mut PendingPrompts, echo: &LocalEcho, msg: UiEventMsg) {
    match msg {
        UiEventMsg::UserMessage(message) => {
            if app.access.is_read_only() {
                apply_wire(app, UiEventMsg::UserMessage(message));
                return;
            }
            if !echo.take(&message) {
                app.push(LineKind::User, message);
            }
            set_running(app, prompts, true);
        }
        UiEventMsg::TurnDone => {
            app.commit_stream();
            // A queued message runs as the next turn without a fresh `UserMessage` (it
            // was journaled when it was queued), so stay running until the queue drains.
            if app.queued.is_empty() {
                set_running(app, prompts, false);
            }
        }
        other => apply_wire(app, other),
    }
}

/// Reconcile running/idle with the worker's published session status. Only the two
/// "is a turn going" states move the view; the awaiting states are driven by prompts.
fn apply_status(app: &mut App, prompts: &mut PendingPrompts, status: SessionStatus) {
    if app.access.is_read_only() {
        return;
    }
    match status {
        SessionStatus::Running => set_running(app, prompts, true),
        SessionStatus::Idle => set_running(app, prompts, false),
        _ => {}
    }
}

/// Move between `Running` and `Idle` without disturbing any other mode. Under an open
/// prompt the change lands in the mode the prompt returns to, so answering an approval
/// after the turn ended doesn't resurrect a spinner.
fn set_running(app: &mut App, prompts: &mut PendingPrompts, running: bool) {
    let (from, to, status) = if running {
        (Mode::Idle, Mode::Running, "running")
    } else {
        (Mode::Running, Mode::Idle, "ready")
    };
    if prompts.active.is_some() {
        if let Some(mode) = prompts.return_mode.as_mut() {
            if *mode == from {
                *mode = to;
            }
        }
        return;
    }
    if app.mode == from {
        app.mode = to;
        app.status = status.into();
    }
}

/// Map a job's wire state (`JobState::as_str`) to the pane's status.
///
/// An unrecognised state — a newer worker's — is shown as running rather than done:
/// "done" stops the timer and drops it from the live count, which would misreport a
/// job that is very much still there.
fn crew_status(state: &str) -> CrewStatus {
    match state {
        "pending" => CrewStatus::Pending,
        "running" => CrewStatus::Running,
        "awaiting verdict" => CrewStatus::Asking,
        "asking a question" => CrewStatus::Waiting,
        "done" => CrewStatus::Done,
        "failed" => CrewStatus::Failed,
        _ => CrewStatus::Running,
    }
}

/// Shared handle to the current turn's cancellation token (set by the agent
/// thread, fired by the TUI's interrupt menu).
pub type TurnCancel = std::sync::Arc<std::sync::Mutex<Option<CancellationToken>>>;

/// Run the conversational TUI event loop. `intro` lines are shown as a welcome
/// banner; `seed` is an optional first message; `task_tx` sends the user's
/// messages to the agent thread (dropping it ends the session); `turn_cancel`
/// interrupts the in-flight turn.
#[allow(clippy::too_many_arguments)]
pub fn run_event_loop(
    title: &str,
    intro: Vec<String>,
    seed: Option<String>,
    events: Receiver<UiEvent>,
    ui_tx: Sender<UiEvent>,
    task_tx: Sender<AgentCmd>,
    turn_cancel: TurnCancel,
    access: Access,
    ctx: SessionCtx,
) -> Result<()> {
    // Keep stray host logs (tracing on stderr) off the alternate screen.
    let _stderr = redirect_stderr_to_log();
    let mut terminal = setup_terminal()?;
    let result = event_loop(
        &mut terminal,
        title,
        intro,
        seed,
        events,
        ui_tx,
        task_tx,
        turn_cancel,
        access,
        ctx,
    );
    restore_terminal(&mut terminal)?;
    result
}

/// Map the wire's approval detail onto the view type.
///
/// The title comes from the *kind* rather than from a string on the wire, so a
/// credential prompt cannot end up titled "Network request" — which is exactly what
/// it was titled while the modal had only a destination label to work from.
fn approval_view(d: ApprovalDetail) -> cowboy_tui::ApprovalView {
    cowboy_tui::ApprovalView {
        once_only: d.kind == ApprovalKind::Credential,
        title: match d.kind {
            ApprovalKind::Network => "Network request".to_string(),
            ApprovalKind::Credential => "Credential access".to_string(),
        },
        rows: d.rows,
        note: d.note,
        summary: String::new(),
    }
}

/// Static session context the slash commands need.
#[derive(Clone)]
pub struct SessionCtx {
    /// Project root (for `/diff`).
    pub root: PathBuf,
    /// Available model names (for `/model`).
    pub models: Vec<String>,
    /// The currently active model name.
    pub current_model: String,
    /// The ranch this session belongs to, if it's a workstream (enables `/accept`).
    pub ranch_id: Option<String>,
    /// The workstream id within the ranch, if any.
    pub workstream_id: Option<String>,
    /// Suggested opening prompts for a fresh session, offered in the welcome banner
    /// and submittable with Alt-1…Alt-9. Empty on attach and whenever the session
    /// already started with a task.
    pub suggestions: Vec<String>,
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Mouse tracking, enabled by hand rather than with crossterm's
/// `EnableMouseCapture`. That helper also turns on **any-motion** reporting
/// (`?1003h`), so every pointer movement across the window becomes ~16 bytes of
/// input — while [`handle_mouse`] only cares about motion with a button held.
/// That idle flood is the cheapest way to overrun crossterm's 1 KiB read buffer
/// and strand input for good (see [`stranded_input_should_drop`]).
/// `?1002h` reports motion only during a drag, which is all the selection needs.
const MOUSE_TRACKING_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
/// The inverse, plus `?1003l` in case something else in the stack enabled
/// any-motion reporting — leaving it on would spam the user's shell.
const MOUSE_TRACKING_OFF: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
/// Save/restore the window title on the terminal's own title stack (XTWINOPS
/// 22/23). We rename the window while a session is waiting on the user, and this
/// is what makes that reversible — without it, quitting would leave the terminal
/// stuck with our title.
const TITLE_PUSH: &str = "\x1b[22;0t";
const TITLE_POP: &str = "\x1b[23;0t";

fn setup_terminal() -> Result<Term> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Capture the mouse so drag-selection is scoped to the transcript panel (not
    // the side panels / borders); `y` then copies just that text via OSC 52 and
    // clears the highlight (any other key dismisses it). Hold Shift to bypass
    // capture for your terminal's native (whole-screen) selection. Bracketed
    // paste stays on so multi-line pastes into the input arrive as one chunk.
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
    stdout.write_all(MOUSE_TRACKING_ON.as_bytes())?;
    stdout.write_all(TITLE_PUSH.as_bytes())?;
    stdout.flush()?;
    // Best-effort: the kitty keyboard protocol lets us distinguish Shift+Enter
    // (newline) from Enter (send). Harmless where unsupported.
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            terminal.backend_mut(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
    terminal::disable_raw_mode()?;
    terminal
        .backend_mut()
        .write_all(MOUSE_TRACKING_OFF.as_bytes())?;
    // Give the window its original title back (see `TITLE_PUSH`).
    terminal.backend_mut().write_all(TITLE_POP.as_bytes())?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Copy `text` to the clipboard via OSC 52 — the terminal-native mechanism, and
/// the *only* clipboard path (a blocking native X11/Wayland backend used to sit in
/// front of this and froze the event loop; see the copy site). Works in Ghostty,
/// kitty, iTerm2, and over SSH/tmux (with passthrough), though some
/// terminal/multiplexer stacks drop it — the terminal must allow clipboard writes.
///
/// `out` MUST be ratatui's own terminal backend (`terminal.backend_mut()`), not
/// a fresh `io::stdout()`: crossterm buffers each frame and flushes it at the
/// end of `draw()`, so a write to an independent stdout handle interleaves with
/// a queued-but-unflushed frame and the OSC 52 bytes get eaten. Routing it
/// through the same writer keeps everything on one buffer with deterministic
/// ordering. The event loop calls this *after* `draw()` has flushed the frame.
fn clipboard_copy(out: &mut impl io::Write, text: &str) {
    // OSC 52: ask the terminal to set the system clipboard. Works over SSH and
    // in most modern terminals (the terminal must allow clipboard writes).
    let osc = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let seq = multiplexer_passthrough(osc);
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Wrap an OSC sequence so it survives a terminal multiplexer.
///
/// Inside tmux/screen an OSC must be wrapped in DCS passthrough or it never
/// reaches the outer terminal — a very common "copy doesn't work" cause, and the
/// same applies to a notification or a window-title write.
fn multiplexer_passthrough(osc: String) -> String {
    if std::env::var_os("TMUX").is_some() {
        // tmux: wrap in `\ePtmux;…\e\\` with inner ESCs doubled.
        format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b"))
    } else if std::env::var_os("STY").is_some() {
        // GNU screen: pass through via DCS.
        format!("\x1bP{osc}\x1b\\")
    } else {
        osc
    }
}

/// Whether out-of-band attention signals (bell, desktop notification, window
/// title) are wanted. On by default; `COWBOY_NOTIFY=0|off|false|""` turns them off
/// for anyone whose terminal or window manager makes them obnoxious.
fn notifications_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("COWBOY_NOTIFY") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "off" | "false"
        ),
        Err(_) => true,
    })
}

/// Tell the user out of band that the session has parked on them.
///
/// The status bar already says so, which is no help when the window isn't
/// focused — and these pauses fail closed, so an unnoticed prompt is a stalled
/// session rather than a slow one. Two signals, both cheap and both ignored by
/// terminals that don't implement them: BEL (near-universal, usually an urgency
/// hint to the window manager) and OSC 9 (a real desktop notification in
/// WezTerm/iTerm2/Windows Terminal/foot).
///
/// Same writer constraint as [`clipboard_copy`]: this must go through ratatui's
/// backend after the frame has flushed.
fn notify_attention(out: &mut impl io::Write, reason: &str) {
    if !notifications_enabled() {
        return;
    }
    let osc = format!("\x1b]9;cowboy — {reason}\x07");
    let _ = out.write_all(multiplexer_passthrough(osc).as_bytes());
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

/// Set the window title, so a background session that needs you is visible in the
/// taskbar / tab strip. Reversible: `setup_terminal` pushes the original onto the
/// terminal's title stack and `restore_terminal` pops it, so we never leave a
/// terminal renamed after exit.
fn set_window_title(out: &mut impl io::Write, context: &str, waiting: bool) {
    if !notifications_enabled() {
        return;
    }
    let text = if waiting {
        format!("⏳ cowboy needs you — {context}")
    } else {
        format!("cowboy — {context}")
    };
    let osc = format!("\x1b]2;{text}\x07");
    let _ = out.write_all(multiplexer_passthrough(osc).as_bytes());
    let _ = out.flush();
}

/// How many terminal events one iteration will consume before yielding to a
/// redraw. High enough that a drag-selection or a paste is absorbed in a single
/// frame, low enough that a runaway input source can't stop the UI updating.
const INPUT_DRAIN_BUDGET: usize = 512;
/// Empty polls with bytes still queued before we call the input stranded. Two is
/// enough: anything that arrived normally re-arms the readiness edge and is read
/// on the very next iteration, so surviving two polls means no edge is coming.
const STRANDED_AFTER_EMPTY_POLLS: u32 = 2;

/// Whether queued-but-unreported terminal input should be dropped to get the UI
/// moving again.
///
/// crossterm registers the tty with mio — i.e. **edge-triggered** epoll — and its
/// read loop returns as soon as its parser yields one event, so it never drains
/// the fd (it can't: stdin is blocking, so the `WouldBlock` break it relies on is
/// unreachable). An input burst larger than its 1 KiB buffer therefore leaves
/// bytes in the kernel queue with no further readiness edge to come, and
/// `event::poll` reports "no input" *forever* while the render loop keeps
/// drawing: the UI looks frozen and even Ctrl-C is dead, because raw mode
/// delivers it as a key event to the loop that cannot read it. Seen in the wild
/// after a long drag-selection — with mouse motion reporting on, one drag is
/// easily 3 KiB of complete SGR reports.
///
/// Recovery has to come from us, because only *new* bytes re-arm the edge and the
/// user's keystrokes may not be reaching the pty at all by then. Once the same
/// queued bytes survive [`STRANDED_AFTER_EMPTY_POLLS`] they are unreachable, so
/// they get dropped: discarding a backlog the user never saw beats a dead UI.
/// (crossterm's parser may be holding the head of a sequence whose tail we drop,
/// which can garble one subsequent event — a cheap price for not wedging.)
fn stranded_input_should_drop(pending: usize, consecutive_empty_polls: u32) -> bool {
    pending > 0 && consecutive_empty_polls >= STRANDED_AFTER_EMPTY_POLLS
}

/// Bytes waiting in the terminal's input queue, or 0 if stdin isn't a tty or the
/// query fails — this only ever triggers recovery, so failing to 0 is the safe
/// direction.
fn tty_pending_bytes() -> usize {
    let mut queued: libc::c_int = 0;
    // SAFETY: both calls take stdin's raw fd; FIONREAD writes one `c_int`
    // through a pointer to a live local.
    unsafe {
        if libc::isatty(libc::STDIN_FILENO) != 1 {
            return 0;
        }
        if libc::ioctl(libc::STDIN_FILENO, libc::FIONREAD, &mut queued) != 0 {
            return 0;
        }
    }
    queued.max(0) as usize
}

/// Read and discard up to `pending` bytes from stdin, returning how many went.
/// Called only when the kernel has just reported that many queued, so the reads
/// cannot block even though stdin is in blocking mode.
fn drop_tty_input(pending: usize) -> usize {
    let mut buf = [0u8; 1024];
    let mut dropped = 0usize;
    while dropped < pending {
        let want = (pending - dropped).min(buf.len());
        // SAFETY: reading into our own buffer, bounded by its length.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), want) };
        if n <= 0 {
            break;
        }
        dropped += n as usize;
    }
    dropped
}

/// Minimal standard base64 encoder (avoids a dependency for OSC 52 payloads).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Redirect the process's stderr to a per-run log file for the lifetime of the
/// TUI, restoring it on drop. Host `tracing` output goes to stderr; without this
/// it would scribble over the alternate-screen UI.
struct StderrGuard {
    _file: std::fs::File,
    saved: i32,
}

impl Drop for StderrGuard {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
        }
    }
}

fn redirect_stderr_to_log() -> Option<StderrGuard> {
    use std::os::fd::AsRawFd;
    let path = std::env::temp_dir().join(format!("cowboy-{}.log", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    unsafe {
        let saved = libc::dup(libc::STDERR_FILENO);
        if saved < 0 {
            return None;
        }
        if libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) < 0 {
            libc::close(saved);
            return None;
        }
        Some(StderrGuard { _file: file, saved })
    }
}

#[allow(clippy::too_many_arguments)]
fn event_loop(
    terminal: &mut Term,
    title: &str,
    intro: Vec<String>,
    seed: Option<String>,
    events: Receiver<UiEvent>,
    ui_tx: Sender<UiEvent>,
    task_tx: Sender<AgentCmd>,
    turn_cancel: TurnCancel,
    access: Access,
    mut session: SessionCtx,
) -> Result<()> {
    let mut app = App::new_with_access(title.to_string(), access);
    let mut prompts = PendingPrompts::default();
    let mut mode_before_overlay = Mode::Idle;
    // Whether a second Ctrl-C would end the session (see `handle_interrupt`).
    let mut quit_armed = false;
    // Whether the one-shot steering tip has been considered this session.
    let mut steering_tip_done = false;
    // Whether the window title currently carries the "needs you" marker, so it is
    // written only on a change rather than every frame.
    let mut title_marked_waiting = false;
    // What this client echoed locally, so the journaled copy isn't rendered twice.
    let echo = LocalEcho::default();
    let mut task_tx = Some(TaskTx {
        tx: task_tx,
        echo: echo.clone(),
    });
    // Submitted-message history for Up/Down recall.
    let mut history: Vec<String> = Vec::new();
    let mut hist_pos: Option<usize> = None;
    // Slash-command autocomplete catalog (built-ins + discovered skills),
    // computed once: skills rarely change mid-session.
    let completion_catalog = build_completion_catalog(&session);

    // Welcome banner (project info) at the top of the transcript. The intro art goes
    // first, so it sits above the project lines and scrolls away with them.
    if !intro.is_empty() {
        // Sized to the transcript pane (the left 68% of the screen, less its borders
        // and the status/input rows), not the terminal: the art must fit its own pane
        // without wrapping, and must not be pushed straight off the top by the
        // welcome lines that follow it.
        let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
        let pane_w = (u32::from(cols) * 68 / 100).saturating_sub(2) as u16;
        // status bar (1) + input box (3) + the transcript's own borders (2).
        let pane_h = rows.saturating_sub(6);
        let frames = crate::banner::intro_frames(&session.root, pane_w, pane_h);
        app.begin_intro(frames, crate::banner::FRAME_MS, now_ms());
    }
    for line in intro {
        app.push(LineKind::Banner, line);
    }
    // Boundary indicator: read once here rather than per frame — it comes from
    // host-owned config that cannot change under a running session.
    if let Some(s) = crate::cmd::sandbox::summary(&session.root) {
        app.boundary = s;
    }

    // Seed the first turn, or start idle awaiting the first message.
    match seed {
        Some(t) if !t.is_empty() => {
            app.push(LineKind::User, t.clone());
            if let Some(tx) = &task_tx {
                history.push(t.clone());
                let _ = tx.send(AgentCmd::Message(t));
            }
            app.mode = Mode::Running;
        }
        _ => app.mode = Mode::Idle,
    }

    // Clipboard is OSC 52 only: copies are written to the terminal as an escape
    // sequence (see `clipboard_copy`), which works locally and over SSH and — unlike
    // a native X11/Wayland backend — cannot block the event loop. No handle to keep
    // alive, so nothing is created here.

    // Byte offset consumed from the watched subagent's journal, reset when the
    // watch target changes, so the nested view tails the file across ticks.
    let mut watch_pos: u64 = 0;
    let mut watch_pos_id = String::new();
    // Consecutive polls that reported no input, for stranded-input detection.
    let mut empty_polls: u32 = 0;

    'main: loop {
        while let Ok(ev) = events.try_recv() {
            match ev {
                UiEvent::Wire(msg) => apply_live_wire(&mut app, &mut prompts, &echo, msg),
                UiEvent::Ask(id, question, options, reply) => {
                    prompts.add(
                        &mut app,
                        UiPrompt::Ask {
                            id,
                            question,
                            options,
                            reply,
                        },
                    );
                }
                UiEvent::Approval(id, dest, detail, reply) => {
                    prompts.add(
                        &mut app,
                        UiPrompt::Approval {
                            id,
                            dest,
                            detail,
                            reply,
                        },
                    );
                }
                UiEvent::ReplacePrompts(replacement) => {
                    prompts.replace(&mut app, replacement);
                }
                UiEvent::AskResolved(id) | UiEvent::ApprovalResolved(id) => {
                    prompts.resolve(&mut app, id);
                }
                UiEvent::Connection(state) => app.connection = state,
                UiEvent::Lifecycle(status) => {
                    app.lifecycle = Some(status.to_string());
                    apply_status(&mut app, &mut prompts, status);
                }
                UiEvent::ModelsFetched(entries) => {
                    if entries.is_empty() {
                        app.push(LineKind::Notice, "no chat models offered by the provider");
                    } else {
                        mode_before_overlay = app.mode.clone();
                        app.model_picker = Some(ModelPicker {
                            entries,
                            filter: String::new(),
                            selected: 0,
                            crew_mode: crate::cmd::crew::crew_enabled(),
                        });
                        app.mode = Mode::ModelPicker;
                    }
                }
                UiEvent::Done => {
                    app.mode = Mode::Done;
                    app.status = "session ended".into();
                    // The worker may die before emitting `SubagentDone` for in-flight
                    // subagents; freeze their timers so they don't tick forever.
                    app.freeze_crew();
                }
            }
        }

        app.tick();
        app.tick_command(now_ms());
        app.tick_crew(now_ms());
        app.tick_turn(now_ms());
        app.tick_attention(now_ms());
        app.tick_intro(now_ms());
        // Tail the watched subagent's journal into its nested view (poll on the
        // tick; the file is small and local).
        if app.mode == Mode::WatchingSubagent {
            if app.watch_id != watch_pos_id {
                watch_pos = 0;
                watch_pos_id = app.watch_id.clone();
            }
            let path = session
                .root
                .join(".cowboy")
                .join("sessions")
                .join(&app.watch_id)
                .join("events.jsonl");
            watch_pos = poll_subagent_journal(&mut app, &path, watch_pos);
        } else if !watch_pos_id.is_empty() {
            watch_pos = 0;
            watch_pos_id.clear();
        }
        // The first time a turn is actually running, say the one thing that is not
        // discoverable from F1: that the prompt is listening. Checked here rather than at
        // the nine places that set `Mode::Running`, and gated on a loop-local flag so the
        // marker file is read at most once per session.
        if !steering_tip_done && app.mode == Mode::Running {
            steering_tip_done = true;
            if let Some(t) = crate::tips::once(crate::tips::STEERING) {
                app.push(LineKind::Notice, t);
            }
        }
        terminal.draw(|f| draw(f, &app))?;

        // Flush any queued clipboard copy. Prefer the direct OS clipboard
        // (reliable locally on X11/Wayland); fall back to OSC 52 — written
        // through ratatui's own backend *after* the frame, so it isn't eaten by
        // crossterm's buffered frame — when there's no local display (SSH).
        // Outcome is reported both in the status line and the per-run log
        // ($TMPDIR/cowboy-<pid>.log) so copy failures are diagnosable.
        // OSC 52 only: ask the terminal to set the clipboard by writing an escape
        // sequence through ratatui's own backend (never a fresh stdout — see
        // `clipboard_copy`). This runs on the event loop, so it must never block:
        // writing a few bytes to the tty cannot. The previous native path
        // (`clipboard_rs::set_text`) *could* block on the X server / clipboard
        // manager, and did — freezing input while the last frame kept rendering,
        // with Ctrl+C dead because raw mode delivers it as a key event to the very
        // loop that was stuck. OSC 52 removes that failure mode entirely and works
        // identically over SSH. Outcome is logged to $TMPDIR/cowboy-<pid>.log.
        if let Some(text) = app.take_pending_copy() {
            let n = text.chars().count();
            clipboard_copy(terminal.backend_mut(), &text);
            eprintln!("[copy] {n} chars -> clipboard (osc52)");
            app.status = format!("copied {n} chars");
        }

        // Out-of-band attention signals, same after-the-frame writer constraint as
        // the clipboard above. The bell/notification is edge-triggered inside
        // `tick_attention`; the window title is level-triggered here, because it
        // has to be *taken back off* once the wait ends.
        if let Some(reason) = app.take_pending_notify() {
            notify_attention(terminal.backend_mut(), &reason);
        }
        let waiting = app.attention_reason().is_some();
        if waiting != title_marked_waiting {
            title_marked_waiting = waiting;
            let context = app.title.clone();
            set_window_title(terminal.backend_mut(), &context, waiting);
        }

        // Terminal input. Everything already queued is drained before the next
        // frame rather than one event per draw: a drag-selection or a multi-KiB
        // paste arrives as hundreds of events, and one-per-frame turns that into
        // seconds of visible lag. Bounded so a flood cannot starve the redraw.
        let mut drained = 0usize;
        while drained < INPUT_DRAIN_BUDGET
            && event::poll(if drained == 0 {
                // The idle cadence is 120 ms; while the intro animates we poll at its
                // frame interval instead, so the animation is smooth without paying
                // for a faster loop for the rest of the session.
                if app.intro_active() {
                    Duration::from_millis(crate::banner::FRAME_MS)
                } else {
                    Duration::from_millis(120)
                }
            } else {
                Duration::ZERO
            })?
        {
            drained += 1;
            let ev = event::read()?;
            // Any key or click settles the intro immediately — nobody should have to
            // watch an animation to get to their prompt.
            if app.intro_active() && !matches!(ev, Event::Resize(_, _)) {
                app.finish_intro();
            }
            let input_before = app.input_text();
            match ev {
                // Ctrl-L: force a full repaint — escape hatch for terminal render
                // artifacts (stale cells some terminals leave behind). Discards
                // ratatui's known-screen state so the next draw rewrites every cell.
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && key.code == KeyCode::Char('l')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    let _ = terminal.clear();
                    app.status = "redrawn".into();
                }
                // A resize can leave stale cells from the old geometry; repaint.
                Event::Resize(_, _) => {
                    let _ = terminal.clear();
                }
                // Ignore key *release* events (kitty protocol reports them).
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let ctx = KeyCtx {
                        prompts: &mut prompts,
                        mode_before_overlay: &mut mode_before_overlay,
                        turn_cancel: &turn_cancel,
                        task_tx: &mut task_tx,
                        ui_tx: &ui_tx,
                        history: &mut history,
                        hist_pos: &mut hist_pos,
                        session: &mut session,
                        quit_armed: &mut quit_armed,
                    };
                    if handle_key(Event::Key(key), key, &mut app, ctx) {
                        break 'main;
                    }
                }
                Event::Mouse(me) => handle_mouse(me, &mut app),
                // Bracketed paste — insert as one chunk only with interactive
                // access and an editable transient mode.
                Event::Paste(text) if accepts_paste(app.access, &app.mode) => {
                    app.input_paste(&text);
                }
                _ => {}
            }
            // Refresh the slash-command autocomplete when the input changed (so
            // navigation keys, which don't touch the text, keep the selection).
            if app.access == Access::Interactive && matches!(app.mode, Mode::Idle | Mode::Running) {
                if app.input_text() != input_before {
                    refresh_completions(&mut app, &completion_catalog);
                }
            } else {
                app.clear_completions();
            }
        }

        // Input the terminal has delivered but crossterm will never report (see
        // `stranded_input_should_drop`): recover instead of sitting frozen.
        if drained == 0 {
            empty_polls = empty_polls.saturating_add(1);
            let pending = if empty_polls >= STRANDED_AFTER_EMPTY_POLLS {
                tty_pending_bytes()
            } else {
                0
            };
            if stranded_input_should_drop(pending, empty_polls) {
                let dropped = drop_tty_input(pending);
                empty_polls = 0;
                // Logged as well as shown: the status line is transient, and this
                // is the fingerprint to look for if a freeze is ever reported again.
                eprintln!(
                    "[input] {pending} byte(s) queued but unreported by crossterm; dropped {dropped}"
                );
                app.status = format!("input recovered ({dropped} stale bytes dropped)");
            }
        } else {
            empty_polls = 0;
        }
    }
    Ok(())
}

/// Build the autocomplete catalog once: the slash-command table + discovered skills.
fn build_completion_catalog(session: &SessionCtx) -> Vec<cowboy_tui::Completion> {
    let skills = help_skills(session);
    help::completions(&help::Ctx {
        workstream: session.workstream_id.is_some(),
        skills: &skills,
    })
}

/// Recompute autocomplete candidates from the current input. Active only while
/// the (single-line) input is `/<partial>` with no space yet; matches by
/// substring with prefix matches first.
fn refresh_completions(app: &mut App, catalog: &[cowboy_tui::Completion]) {
    let input = app.input_text();
    let partial = match input.strip_prefix('/') {
        Some(rest) if input.lines().count() <= 1 && !rest.contains(char::is_whitespace) => {
            rest.to_lowercase()
        }
        _ => {
            app.clear_completions();
            return;
        }
    };
    let mut items: Vec<cowboy_tui::Completion> = catalog
        .iter()
        .filter(|c| c.value.to_lowercase().contains(&partial))
        .cloned()
        .collect();
    // Prefix matches first, then by name.
    items.sort_by(|a, b| {
        let ap = a.value.to_lowercase().starts_with(&partial);
        let bp = b.value.to_lowercase().starts_with(&partial);
        bp.cmp(&ap).then(a.value.cmp(&b.value))
    });
    app.set_completions(items);
}

/// Mouse → transcript-scoped selection (drag to select; press `y` to copy) +
/// Read newly-appended complete lines from a watched subagent's `events.jsonl`
/// (starting at byte `pos`), applying each to the nested view, and return the new
/// offset. Only advances past `\n`-terminated lines, so a partial trailing line is
/// re-read once the writer finishes it.
fn poll_subagent_journal(app: &mut App, path: &std::path::Path, pos: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return pos;
    };
    if f.seek(SeekFrom::Start(pos)).is_err() {
        return pos;
    }
    let mut tail = String::new();
    if f.read_to_string(&mut tail).is_err() || tail.is_empty() {
        return pos;
    }
    let mut last_nl = 0usize;
    for (i, b) in tail.bytes().enumerate() {
        if b == b'\n' {
            if let Ok(ev) = serde_json::from_str::<UiEventMsg>(tail[last_nl..i].trim_end()) {
                if let Some(sub) = app.watching.as_deref_mut() {
                    apply_wire(sub, ev);
                }
            }
            last_nl = i + 1;
        }
    }
    pos + last_nl as u64
}

/// wheel scroll.
fn handle_mouse(me: crossterm::event::MouseEvent, app: &mut App) {
    use crossterm::event::{MouseButton, MouseEventKind};
    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => app.begin_selection(me.column, me.row),
        MouseEventKind::Drag(MouseButton::Left) => app.drag_selection(me.column, me.row),
        // Some terminals (and some crossterm/terminal combos) report button-held
        // motion as `Moved` rather than `Drag`. While a drag is in progress,
        // treat it as a selection extend so the cursor actually moves.
        MouseEventKind::Moved if app.selecting => app.drag_selection(me.column, me.row),
        // Keep the highlight after release; `y` copies it (vim-style yank).
        MouseEventKind::Up(MouseButton::Left) => {
            app.end_selecting();
            if app.has_selection() {
                app.status = "y: copy selection · Esc: clear".into();
            }
        }
        // Wheel scrolls the view but keeps any selection — it's anchored to
        // logical lines, so it survives scrolling (and can be extended after).
        MouseEventKind::ScrollUp => app.scroll_up(3),
        MouseEventKind::ScrollDown => app.scroll_down(3),
        _ => {}
    }
}

use cowboy_core::time::now_ms;

/// Mutable context handed to the key handler.
struct KeyCtx<'a> {
    prompts: &'a mut PendingPrompts,
    mode_before_overlay: &'a mut Mode,
    turn_cancel: &'a TurnCancel,
    /// `None` once the session has been ended (sender dropped).
    task_tx: &'a mut Option<TaskTx>,
    /// For posting client-side async results (e.g. the fetched model list).
    ui_tx: &'a Sender<UiEvent>,
    history: &'a mut Vec<String>,
    hist_pos: &'a mut Option<usize>,
    session: &'a mut SessionCtx,
    /// Set by the first Ctrl-C on an empty idle prompt, cleared by any other key.
    ///
    /// Ending a session used to need a menu, so it could afford to be one keystroke. A
    /// bare Ctrl-C cannot: it is the key you hit reflexively, and reflexively ending the
    /// session is not recoverable. So the second press is the decision, and anything in
    /// between disarms it.
    quit_armed: &'a mut bool,
}

/// Ctrl-C. Returns true if the loop should exit.
///
/// What it does depends on what is happening, which is the whole point of dropping the
/// menu: the menu made you choose between "resume", "instruct" and "kill" when in every
/// case you had already decided by pressing the key.
fn handle_interrupt(app: &mut App, ctx: &mut KeyCtx) -> bool {
    // 1. Something is running: stop it. Background subagents are left alone — `Alt-s`
    //    stops those, and conflating them means one impatient keystroke discards work
    //    that had nothing to do with the turn being corrected.
    if matches!(app.mode, Mode::Running) {
        if let Some(tok) = ctx.turn_cancel.lock().unwrap().as_ref() {
            tok.cancel();
        }
        *ctx.quit_armed = false;
        app.status = "interrupting — say what to do instead".into();
        // Drop straight to Idle with the input focused, so redirecting is one keystroke
        // and a sentence rather than a menu choice followed by a sentence. The
        // conversation is intact, so the next message continues it.
        app.mode = Mode::Idle;
        return false;
    }
    // 2. Nothing running, but something typed: clear the draft.
    if !app.input_text().trim().is_empty() {
        app.set_input("");
        *ctx.quit_armed = false;
        app.status = "cleared".into();
        return false;
    }
    // 3. Idle and empty: arm, then end.
    if *ctx.quit_armed {
        if let Some(tx) = ctx.task_tx.as_ref() {
            // Explicit `End` before dropping the sender: the hangup route works too, but a
            // side effect of a drop is invisible in a log and was the prime suspect the
            // last time ending a session left a worker running.
            let _ = tx.send(AgentCmd::End);
        }
        ctx.task_tx.take();
        if let Some(tok) = ctx.turn_cancel.lock().unwrap().as_ref() {
            tok.cancel();
        }
        *ctx.quit_armed = false;
        app.status = "ending session…".into();
        return false;
    }
    *ctx.quit_armed = true;
    app.status = "press Ctrl-C again to end the session · Alt-d detaches instead".into();
    false
}

/// Open the help overlay, remembering the mode to return to.
fn open_help(app: &mut App, ctx: &mut KeyCtx, query: Option<&str>) {
    // Don't stash an overlay as the thing to come back to, or dismissing help would
    // reopen a modal whose reply channel is long gone.
    if matches!(app.mode, Mode::Idle | Mode::Running) {
        *ctx.mode_before_overlay = app.mode.clone();
    }
    let skills = help_skills(ctx.session);
    let hctx = help::Ctx {
        workstream: ctx.session.workstream_id.is_some(),
        skills: &skills,
    };
    let sections = help::sections(&hctx, query);
    if sections.is_empty() {
        app.push(
            LineKind::Notice,
            format!(
                "nothing in help matches {:?} — F1 shows everything",
                query.unwrap_or("")
            ),
        );
        return;
    }
    app.open_help(sections, query.map(str::to_string));
}

fn scroll_help(app: &mut App, delta: isize) {
    if let Some(h) = app.help.as_mut() {
        h.scroll_by(delta);
    }
}

/// This project's skills as `(name, hint)`, for help and autocomplete.
fn help_skills(session: &SessionCtx) -> Vec<(String, String)> {
    cowboy_core::skills::discover(&session.root)
        .into_iter()
        .map(|s| {
            let hint = s.argument_hint.clone().unwrap_or_else(|| {
                s.description
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            });
            (s.name, hint)
        })
        .collect()
}

fn read_only_exit_key(access: Access, key: KeyCode) -> Option<bool> {
    access
        .is_read_only()
        .then_some(matches!(key, KeyCode::Char('q') | KeyCode::Esc))
}

fn accepts_paste(access: Access, mode: &Mode) -> bool {
    access == Access::Interactive
        && matches!(mode, Mode::Idle | Mode::Running | Mode::AwaitingInput(_))
}

/// Returns true if the loop should exit.
fn handle_key(event: Event, key: KeyEvent, app: &mut App, mut ctx: KeyCtx) -> bool {
    // Slash-command autocomplete popup: Up/Down navigate, Tab accepts, Esc
    // dismisses. (Enter falls through to submit what's typed; typing refines.)
    if app.has_completions() {
        match key.code {
            KeyCode::Up => {
                app.completion_move(-1);
                return false;
            }
            KeyCode::Down => {
                app.completion_move(1);
                return false;
            }
            KeyCode::Tab => {
                app.accept_completion();
                return false;
            }
            KeyCode::Esc => {
                app.clear_completions();
                return false;
            }
            _ => {}
        }
    }

    // A live transcript selection captures `y` (copy, vim-style) and `Esc`
    // (clear); any other key dismisses the highlight, then proceeds normally — so
    // the selection is always a transient, explicit copy gesture (never lingers).
    if app.has_selection() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Selection is in logical (wrapped-line) coords; extraction
                // renders the transcript off-screen, so it captures the whole
                // selected range even when it spans the scrollback.
                match app.selected_text() {
                    Some(text) => app.request_copy(text),
                    None => app.status = "copy: nothing under the selection".into(),
                }
                // Resume following the tail if we were before selecting.
                app.finish_selection();
                return false;
            }
            KeyCode::Esc => {
                app.finish_selection();
                return false;
            }
            _ => app.clear_selection(),
        }
    }

    // Watching a subagent: Esc returns to the main session, `w` cycles to the
    // next subagent, and scroll keys move the *nested* view. Handled before the
    // global scrollback so those keys target the watched transcript.
    if app.mode == Mode::WatchingSubagent {
        match key.code {
            KeyCode::Esc => app.stop_watching(),
            KeyCode::Char('w') => {
                if let Some((id, label)) = app.next_watch_target() {
                    app.watch_subagent(id, label);
                }
            }
            KeyCode::PageUp => {
                if let Some(s) = app.watching.as_mut() {
                    s.scroll_up(10);
                }
            }
            KeyCode::PageDown => {
                if let Some(s) = app.watching.as_mut() {
                    s.scroll_down(10);
                }
            }
            _ => {}
        }
        return false;
    }

    // Transcript scrollback — works in any mode so you can read while the agent
    // runs or while typing. Uses keys the text editor doesn't claim.
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::PageUp => {
            app.clear_selection();
            app.scroll_up(10);
            return false;
        }
        KeyCode::PageDown => {
            app.clear_selection();
            app.scroll_down(10);
            return false;
        }
        KeyCode::Up if shift => {
            app.clear_selection();
            app.scroll_up(1);
            return false;
        }
        KeyCode::Down if shift => {
            app.clear_selection();
            app.scroll_down(1);
            return false;
        }
        KeyCode::End if shift => {
            app.clear_selection();
            app.scroll_to_bottom();
            return false;
        }
        _ => {}
    }

    // Persistent read-only access is orthogonal to the transient mode. After
    // navigation/copy, the only local actions are help and exit; nothing below
    // this gate may edit input, answer prompts, interrupt, detach through the
    // command channel, or otherwise mutate the session.
    if app.access.is_read_only() {
        if key.code == KeyCode::F(1) && app.mode != Mode::Help {
            *ctx.mode_before_overlay = app.mode.clone();
            open_help(app, &mut ctx, None);
            return false;
        }
        if app.mode == Mode::Help {
            match key.code {
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Enter | KeyCode::Char('q') => {
                    app.close_help(ctx.mode_before_overlay.clone());
                }
                KeyCode::Up => scroll_help(app, -1),
                KeyCode::Down => scroll_help(app, 1),
                KeyCode::PageUp => scroll_help(app, -10),
                KeyCode::PageDown => scroll_help(app, 10),
                KeyCode::Home => scroll_help(app, isize::MIN / 2),
                KeyCode::End => scroll_help(app, isize::MAX / 2),
                _ => {}
            }
            return false;
        }
        return read_only_exit_key(app.access, key.code).unwrap_or(false);
    }

    // Ctrl-C acts, rather than opening a menu to act from.
    //
    // Three meanings, picked from what is on screen rather than from a submenu — this is
    // the terminal convention, and each is the obvious response to the situation:
    // interrupt what is running; else clear what you typed; else (twice) end the session.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return handle_interrupt(app, &mut ctx);
    }
    // Any other key disarms a pending "press again to end".
    if *ctx.quit_armed {
        *ctx.quit_armed = false;
    }

    // The help overlay: F1 anywhere, Esc to leave. Purely a view; the turn keeps running
    // behind it, so it is safe to open mid-flight and nothing is queued up waiting on it.
    if key.code == KeyCode::F(1) && app.mode != Mode::Help {
        open_help(app, &mut ctx, None);
        return false;
    }
    if app.mode == Mode::Help {
        match key.code {
            KeyCode::Esc | KeyCode::F(1) | KeyCode::Enter | KeyCode::Char('q') => {
                app.close_help(ctx.mode_before_overlay.clone());
            }
            KeyCode::Up => scroll_help(app, -1),
            KeyCode::Down => scroll_help(app, 1),
            KeyCode::PageUp => scroll_help(app, -10),
            KeyCode::PageDown => scroll_help(app, 10),
            KeyCode::Home => scroll_help(app, isize::MIN / 2),
            KeyCode::End => scroll_help(app, isize::MAX / 2),
            _ => {}
        }
        return false;
    }

    // Direct hotkeys for what used to be behind Ctrl-C. Alt- rather than Ctrl- or bare
    // letters: a bare letter is typed text in the editing modes, and the Ctrl- space is
    // almost entirely claimed by the input editor's own bindings.
    if key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            // What is running in the background, answered from local state — no round
            // trip, and nothing added to the conversation the model has to read.
            KeyCode::Char('j') => {
                app.push(LineKind::Notice, app.jobs_summary());
                return false;
            }
            // Stop the delegated work, leaving this turn running. Deliberately separate
            // from interrupting: throwing away minutes of subagent work to correct one
            // sentence was the behaviour worth avoiding.
            KeyCode::Char('s') => {
                if let Some(tx) = ctx.task_tx.as_ref() {
                    let _ = tx.send(AgentCmd::StopSubagents);
                }
                app.status = "stopping subagents…".into();
                return false;
            }
            KeyCode::Char('w') => {
                match app.next_watch_target() {
                    Some((id, label)) => app.watch_subagent(id, label),
                    None => app.status = "no subagents to watch".into(),
                }
                return false;
            }
            // Detach: leave the session running and exit this client.
            KeyCode::Char('d') => {
                if let Some(tx) = ctx.task_tx.as_ref() {
                    let _ = tx.send(AgentCmd::Detach);
                }
                app.status = "detaching…".into();
                return true; // exit the event loop; the worker keeps running
            }
            // Launchpad: start with one of the suggested openers from the welcome
            // banner. Alt- rather than a bare digit because a bare digit is the
            // first character of plenty of real messages.
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                match ctx.session.suggestions.get(i).cloned() {
                    Some(prompt) => send_message(app, &mut ctx, prompt),
                    None => app.status = format!("no suggestion {c}"),
                }
                return false;
            }
            // Fold/unfold the turns you have already read.
            KeyCode::Char('f') => {
                app.status = app.toggle_folds();
                return false;
            }
            _ => {}
        }
    }

    // Approval modal (network egress, or a credential mount/injection).
    if let Mode::Approval(_) = &app.mode {
        let decision = match key.code {
            KeyCode::Char('o') => Some((Verdict::Allow, ApprovalScope::Once)),
            KeyCode::Char('s') => Some((Verdict::Allow, ApprovalScope::Session)),
            KeyCode::Char('p') => Some((Verdict::Allow, ApprovalScope::Project)),
            KeyCode::Char('g') => Some((Verdict::Allow, ApprovalScope::Global)),
            KeyCode::Char('d') | KeyCode::Esc => Some((Verdict::Deny, ApprovalScope::Once)),
            _ => None,
        };
        if let Some(decision) = decision {
            ctx.prompts.answer_approval(app, decision);
        }
        return false;
    }

    // Model picker: navigate / filter / select.
    if app.mode == Mode::ModelPicker {
        handle_picker_key(key, app, &mut ctx);
        return false;
    }
    // Model config form: edit fields / save / cancel.
    if app.mode == Mode::ModelForm {
        handle_form_key(key, app, &mut ctx);
        return false;
    }

    let editing = matches!(
        app.mode,
        Mode::Idle | Mode::Running | Mode::AwaitingInput(_)
    );
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Modified Enter inserts a newline (multi-line input); plain Enter sends.
    if editing && key.code == KeyCode::Enter && (shift || alt) {
        app.input_newline();
        return false;
    }
    // Up/Down recall message history at the top/bottom edge of the input.
    if editing && key.modifiers.is_empty() {
        if key.code == KeyCode::Up && app.input_cursor_row() == 0 {
            history_recall_prev(app, ctx.history, ctx.hist_pos);
            return false;
        }
        if key.code == KeyCode::Down && app.input_cursor_row() + 1 >= app.input_lines() {
            history_recall_next(app, ctx.history, ctx.hist_pos);
            return false;
        }
    }

    match (&app.mode, key.code) {
        (Mode::Done, _) => return true,
        (Mode::AwaitingInput(_), KeyCode::Enter) => {
            let answer = app.take_input();
            app.push(LineKind::User, answer.clone());
            ctx.prompts.answer_ask(app, answer);
            app.status = "running".into();
        }
        // Multiple-choice question: arrows move, digits pick, Enter chooses, and
        // typing (then Enter) submits a free-form "other" answer.
        (Mode::AwaitingChoice, KeyCode::Up) => app.choice_move(-1),
        (Mode::AwaitingChoice, KeyCode::Down) => app.choice_move(1),
        (Mode::AwaitingChoice, KeyCode::Char(d))
            if d.is_ascii_digit() && d != '0' && app.input_is_empty() =>
        {
            if let Some(answer) = app.choice_option(d as usize - '1' as usize) {
                app.push(LineKind::User, answer.clone());
                ctx.prompts.answer_ask(app, answer);
                app.status = "running".into();
            }
        }
        (Mode::AwaitingChoice, KeyCode::Enter) => {
            let answer = app.choice_answer();
            app.push(LineKind::User, answer.clone());
            ctx.prompts.answer_ask(app, answer);
            app.status = "running".into();
        }
        (Mode::AwaitingChoice, _) => app.input_event(event),
        // Submit a message (Idle or while a turn is running -> queued).
        (Mode::Idle | Mode::Running, KeyCode::Enter) => {
            // A new turn streams output; drop any stray selection so its
            // highlight can't linger over the incoming text.
            app.clear_selection();
            let msg = app.take_input();
            let trimmed = msg.trim();
            if trimmed.is_empty() {
                // nothing to do
            } else if let Some(rest) = trimmed.strip_prefix('/') {
                if handle_command(rest, app, &mut ctx) {
                    return true;
                }
            } else {
                send_message(app, &mut ctx, msg);
            }
        }
        // Everything else is text input for the editor.
        _ => app.input_event(event),
    }
    false
}

/// Send `msg` to the agent as a user turn: echo it, record it in history, and mark a
/// turn in flight. Shared by Enter-to-submit and the launchpad hotkeys so the two
/// cannot drift.
fn send_message(app: &mut App, ctx: &mut KeyCtx, msg: String) {
    let Some(tx) = ctx.task_tx.as_ref() else {
        return;
    };
    app.push(LineKind::User, msg.clone());
    ctx.history.push(msg.clone());
    *ctx.hist_pos = None;
    let _ = tx.send(AgentCmd::Message(msg));
    app.mode = Mode::Running;
    app.status = "running".into();
}

fn boundary_command(app: &mut App, root: &std::path::Path) {
    push_report(app, super::commands::boundary_report(root));
}

/// Render the latest context-window snapshot.
///
/// Reads state the agent loop already reports every turn rather than asking the worker
/// for it, so this works while a turn is running and needs no round trip. Before the
/// first request there is nothing to show, and saying so is better than showing zeroes
/// that look like an empty context.
fn context_command(app: &mut App) {
    let Some(c) = app.context.clone() else {
        app.push(
            LineKind::Notice,
            "no context snapshot yet — send a message first",
        );
        return;
    };
    let pct = c.percent();
    app.push(
        LineKind::Notice,
        format!(
            "context  {}/{} tokens of the conversation budget ({pct}%)",
            fmt_thousands(c.used),
            fmt_thousands(c.budget)
        ),
    );
    app.push(
        LineKind::Notice,
        format!(
            "         window {} · reserved {} for the reply, tool schemas and headroom",
            fmt_thousands(c.window),
            fmt_thousands(c.reserve)
        ),
    );
    if pct >= 80 {
        app.push(
            LineKind::Notice,
            "         near the budget — older turns will be compacted into a summary",
        );
    }
    if c.top.is_empty() {
        return;
    }
    app.push(LineKind::Notice, "         largest first:");
    for (label, n) in &c.top {
        // A bar makes the ratio readable at a glance; the number is there for when it
        // matters.
        let share = if c.used > 0 {
            (*n * 20 / c.used.max(1)).min(20)
        } else {
            0
        };
        let bar: String = "█".repeat(share as usize);
        app.push(
            LineKind::Notice,
            format!("           {:<20} {:>9}  {bar}", label, fmt_thousands(*n)),
        );
    }
}

/// `123456` -> `123,456`, so a five-digit token count is readable.
fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn mcp_command(app: &mut App, root: &std::path::Path) {
    push_report(app, super::commands::mcp_report(root));
}

fn crew_command(arg: Option<&str>, app: &mut App) {
    push_report(app, super::commands::crew_report(arg));
}

/// Handle a `/command`. Returns `true` if the client should exit the event loop
/// now (e.g. `/detach`).
fn handle_command(input: &str, app: &mut App, ctx: &mut KeyCtx) -> bool {
    let mut parts = input.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next();
    match cmd {
        "help" | "h" | "?" => open_help(app, ctx, arg),
        "clear" => {
            app.transcript.clear();
            app.activity.clear();
            app.streaming.clear();
            app.clear_selection();
            app.scroll_to_bottom();
            app.push(
                LineKind::Notice,
                "cleared the view (conversation memory kept)",
            );
        }
        "diff" => {
            let out = git_diff(&ctx.session.root);
            if out.trim().is_empty() {
                app.push(LineKind::Notice, "no working-tree changes");
            } else {
                app.push(LineKind::Command, "git diff");
                for line in out.lines() {
                    app.push(LineKind::Output, line.to_string());
                }
            }
        }
        "copy" => match last_answer(app) {
            // Outcome/status is set by the event-loop drain once it runs.
            Some(text) => app.request_copy(text),
            None => app.push(LineKind::Notice, "nothing to copy yet"),
        },
        "model" => match arg {
            None => {
                app.push(
                    LineKind::Notice,
                    format!(
                        "model: {} (available: {})",
                        ctx.session.current_model,
                        ctx.session.models.join(", ")
                    ),
                );
            }
            // An attached TUI isn't told the model list (the worker validates the
            // switch itself), so only refuse a name when there is a list to check.
            Some(name)
                if !ctx.session.models.is_empty()
                    && !ctx.session.models.iter().any(|m| m == name) =>
            {
                app.push(
                    LineKind::Error,
                    format!(
                        "unknown model {name:?}; available: {}",
                        ctx.session.models.join(", ")
                    ),
                );
            }
            Some(name) => {
                if let Some(tx) = ctx.task_tx {
                    let _ = tx.send(AgentCmd::SwitchModel(name.to_string()));
                    ctx.session.current_model = name.to_string();
                    app.push(
                        LineKind::Notice,
                        format!("model → {name} (from the next turn)"),
                    );
                }
            }
        },
        "models" => {
            // Fetch the provider catalogue off-thread; the picker opens when the
            // ModelsFetched event arrives.
            app.push(LineKind::Notice, "fetching models…");
            spawn_model_fetch(ctx.ui_tx.clone(), ctx.session.current_model.clone());
        }
        "plan" => {
            let task = input.strip_prefix("plan").unwrap_or("").trim();
            if task.is_empty() {
                app.push(
                    LineKind::Notice,
                    "usage: /plan <task> — the agent proposes a plan first; \
                     file edits stay blocked until you approve with /go",
                );
            } else if let Some(tx) = ctx.task_tx.as_ref() {
                let _ = tx.send(AgentCmd::PlanMode(true));
                app.plan_mode = true;
                let prompt = super::commands::plan_prompt(task);
                app.push(LineKind::User, format!("/{input}"));
                let _ = tx.send(AgentCmd::Message(prompt));
                app.mode = Mode::Running;
                app.status = "planning…".into();
            }
        }
        "go" => {
            let note = input.strip_prefix("go").unwrap_or("").trim();
            if let Some(tx) = ctx.task_tx.as_ref() {
                let _ = tx.send(AgentCmd::PlanMode(false));
                app.plan_mode = false;
                app.push(LineKind::User, format!("/{input}"));
                let _ = tx.send(AgentCmd::Message(super::commands::go_prompt(note)));
                app.mode = Mode::Running;
                app.status = "executing…".into();
            }
        }
        "accept" => {
            // Sign off on this ranch workstream: complete it, advance the plan, and
            // end the session. Only valid inside a workstream session.
            if ctx.session.workstream_id.is_none() {
                app.push(
                    LineKind::Notice,
                    "/accept only applies to a ranch workstream session",
                );
            } else if let Some(tx) = ctx.task_tx.as_ref() {
                let note = input.strip_prefix("accept").unwrap_or("").trim();
                let note = (!note.is_empty()).then(|| note.to_string());
                app.push(LineKind::User, format!("/{input}"));
                let _ = tx.send(AgentCmd::Accept { note });
                app.status = "signing off…".into();
            }
        }
        "ranch" => {
            // Bridge: turn the current (single-session) discussion into a
            // multi-workstream ranch using the context already built — no need
            // to re-run `cowboy ranch plan`.
            if let Some(tx) = ctx.task_tx.as_ref() {
                let note = input.strip_prefix("ranch").unwrap_or("").trim();
                app.push(LineKind::User, format!("/{input}"));
                let _ = tx.send(AgentCmd::Message(super::commands::ranch_prompt(note)));
                app.mode = Mode::Running;
                app.status = "drafting a ranch…".into();
            }
        }
        "crew" => crew_command(arg, app),
        // Read from the state the worker already publishes, so both work while a turn is
        // running and need no round trip.
        "jobs" => app.push(LineKind::Notice, app.jobs_summary()),
        "queue" => match arg.map(str::trim) {
            Some("clear") => {
                if let Some(tx) = ctx.task_tx.as_ref() {
                    let _ = tx.send(AgentCmd::QueueClear);
                }
                app.push(LineKind::Notice, "clearing the queued messages…");
            }
            _ if app.queued.is_empty() => {
                app.push(
                    LineKind::Notice,
                    "nothing queued — while the agent works, typing steers the current \
                     turn; use /after <msg> to queue instead",
                );
            }
            _ => {
                let mut s = format!("{} queued message(s):", app.queued.len());
                for (i, q) in app.queued.iter().enumerate() {
                    s.push_str(&format!("\n  {}. {q}", i + 1));
                }
                s.push_str("\n/queue clear drops them");
                app.push(LineKind::Notice, s);
            }
        },
        // Deferral, as opposed to steering: this runs as its own turn afterwards.
        "after" | "then" => match arg.map(str::trim).filter(|s| !s.is_empty()) {
            Some(text) => {
                if let Some(tx) = ctx.task_tx.as_ref() {
                    let _ = tx.send(AgentCmd::Enqueue(text.to_string()));
                }
                app.push(LineKind::User, format!("/{input}"));
                app.push(
                    LineKind::Notice,
                    "queued to run after the current turn (/queue to review)",
                );
            }
            None => app.push(
                LineKind::Notice,
                "usage: /after <message> — queues it to run after the current turn",
            ),
        },
        "mcp" => mcp_command(app, &ctx.session.root),
        "context" => context_command(app),
        // Collapse the mechanics of turns you have already read, so a long session
        // can be scanned as a conversation rather than scrolled through as a log.
        "fold" => {
            let n = app.fold_completed_turns();
            app.status = if n == 0 {
                "nothing to fold yet".into()
            } else {
                format!("folded {n} earlier turn{}", if n == 1 { "" } else { "s" })
            };
        }
        "unfold" => {
            let n = app.unfold_all();
            app.status = format!("expanded {n} turn{}", if n == 1 { "" } else { "s" });
        }
        "boundary" => boundary_command(app, &ctx.session.root),
        // Expanded by the worker (shared with the web client); its reply comes back
        // as a notice.
        "budget" => {
            if let Some(tx) = ctx.task_tx.as_ref() {
                let _ = tx.send(AgentCmd::Command(input.to_string()));
            }
        }
        "stop" => {
            if let Some(tx) = ctx.task_tx.as_ref() {
                let _ = tx.send(AgentCmd::StopSubagents);
            }
            app.push(LineKind::Notice, "stopping the background subagents…");
        }
        "quit" | "exit" | "q" | "end" => {
            ctx.task_tx.take();
            if let Some(tok) = ctx.turn_cancel.lock().unwrap().as_ref() {
                tok.cancel();
            }
            app.status = "ending session…".into();
        }
        "detach" => {
            // Leave the session running; exit this client for later re-attach.
            if let Some(tx) = ctx.task_tx.as_ref() {
                let _ = tx.send(AgentCmd::Detach);
            }
            app.status = "detaching…".into();
            return true;
        }
        "skills" => {
            let skills = cowboy_core::skills::discover(&ctx.session.root);
            if skills.is_empty() {
                app.push(
                    LineKind::Notice,
                    "no skills found (.cowboy/skills or .claude/skills)",
                );
            } else {
                app.push(LineKind::Notice, "skills (run with `/<name> [args]`):");
                for s in skills {
                    let hint = s.argument_hint.map(|h| format!(" {h}")).unwrap_or_default();
                    app.push(
                        LineKind::Notice,
                        format!("  /{}{hint}  — {}", s.name, s.description),
                    );
                }
            }
        }
        other => {
            // A user-invocable skill? Run it: send its instructions (with
            // `$ARGUMENTS` filled in) as the turn so the agent follows them.
            if let Some(skill) = cowboy_core::skills::load(&ctx.session.root, other) {
                let args = input.get(other.len()..).unwrap_or("").trim();
                let prompt = super::commands::skill_prompt(&skill, args);
                if let Some(tx) = ctx.task_tx.as_ref() {
                    app.push(LineKind::User, format!("/{input}"));
                    let _ = tx.send(AgentCmd::Message(prompt));
                    app.mode = Mode::Running;
                    app.status = format!("running skill {}", skill.name);
                }
            } else {
                // Suggest, rather than just refusing: a mistyped command is nearly always
                // one edit away from a real one, and the alternative is the user opening
                // help to scan twenty rows for the name they almost typed.
                let hint = match help::nearest(other) {
                    Some(c) => format!("unknown command /{other} — did you mean /{c}?"),
                    None => format!("unknown command /{other} — press F1 for the list"),
                };
                app.push(LineKind::Error, hint);
            }
        }
    }
    false
}

// --- /models: catalogue picker + config form ------------------------------

/// Fetch the provider catalogue off the UI thread and post the result.
fn spawn_model_fetch(ui_tx: Sender<UiEvent>, current_name: String) {
    std::thread::spawn(move || match fetch_model_choices(&current_name) {
        Ok(choices) => {
            let _ = ui_tx.send(UiEvent::ModelsFetched(choices));
        }
        Err(e) => {
            let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(format!(
                "model list failed: {e}"
            ))));
        }
    });
}

/// Query every configured provider's `/models`, filter to chat models, and join
/// with the shipped defaults + existing config into picker choices.
fn fetch_model_choices(current_name: &str) -> Result<Vec<ModelChoice>> {
    use cowboy_core::config::{expand_env, ConfigPaths, ModelsConfig, ProvidersConfig};
    use cowboy_core::model::list_models;
    use cowboy_core::model_defaults;

    let providers = ProvidersConfig::load_global()?;
    if providers.providers.is_empty() {
        anyhow::bail!("no providers configured; run `cowboy models setup`");
    }
    // Existing config (user + project) for the configured/current markers.
    let user = ModelsConfig::user_path().and_then(|p| ModelsConfig::load_opt(&p).ok().flatten());
    let project = ConfigPaths::for_root(crate::cmd::project_root().unwrap_or_default());
    let project = ModelsConfig::load_opt(&project.models).ok().flatten();
    let mut id_to_name: std::collections::BTreeMap<String, String> = Default::default();
    let mut name_to_id: std::collections::BTreeMap<String, String> = Default::default();
    for cfg in [user.as_ref(), project.as_ref()].into_iter().flatten() {
        for (k, d) in &cfg.models {
            id_to_name
                .entry(d.model.clone())
                .or_insert_with(|| k.clone());
            name_to_id.insert(k.clone(), d.model.clone());
        }
    }
    let current_id = name_to_id.get(current_name).cloned();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<ModelChoice> = Vec::new();
    for p in providers.providers.values() {
        let base = expand_env(&p.base_url).unwrap_or_else(|_| p.base_url.clone());
        let entries = rt.block_on(list_models(&base, &p.api_key, &p.headers))?;
        for e in entries {
            if !model_defaults::is_chat(&e.id) || !seen.insert(e.id.clone()) {
                continue;
            }
            let d = model_defaults::lookup(&e.id);
            let configured_name = id_to_name.get(&e.id).cloned();
            let current = current_id.as_deref() == Some(e.id.as_str());
            out.push(ModelChoice {
                label: configured_name.clone().unwrap_or_else(|| d.name.clone()),
                configured: configured_name.is_some(),
                current,
                configured_name,
                suggested_name: d.name,
                context_window: d.context_window,
                max_tokens: d.max_tokens,
                temperature: d.temperature,
                reasoning: d.reasoning_effort.map(|r| r.as_str().to_string()),
                id: e.id,
            });
        }
    }
    // Configured models the provider catalogue didn't return (a stale
    // `/v1/models` that omits a model you've set up — common with some gateways/
    // providers). Include them so they're always selectable.
    for cfg in [user.as_ref(), project.as_ref()].into_iter().flatten() {
        for (name, def) in &cfg.models {
            if !seen.insert(def.model.clone()) {
                continue; // already listed from a provider catalogue
            }
            let d = model_defaults::lookup(&def.model);
            let current = current_id.as_deref() == Some(def.model.as_str());
            out.push(ModelChoice {
                label: name.clone(),
                configured: true,
                current,
                configured_name: Some(name.clone()),
                suggested_name: d.name,
                context_window: d.context_window,
                max_tokens: d.max_tokens,
                temperature: d.temperature,
                reasoning: d.reasoning_effort.map(|r| r.as_str().to_string()),
                id: def.model.clone(),
            });
        }
    }
    // Current first, then configured, then alphabetical.
    out.sort_by(|a, b| {
        b.current
            .cmp(&a.current)
            .then(b.configured.cmp(&a.configured))
            .then(a.label.cmp(&b.label))
    });
    Ok(out)
}

fn handle_picker_key(key: KeyEvent, app: &mut App, ctx: &mut KeyCtx) {
    let Some(p) = app.model_picker.as_mut() else {
        app.mode = ctx.mode_before_overlay.clone();
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.model_picker = None;
            app.mode = ctx.mode_before_overlay.clone();
        }
        KeyCode::Up => p.move_sel(-1),
        KeyCode::Down => p.move_sel(1),
        // Tab toggles Solo ⇄ Crew for the selection.
        KeyCode::Tab => p.crew_mode = !p.crew_mode,
        KeyCode::Backspace => {
            p.filter.pop();
            p.clamp();
        }
        KeyCode::Enter => {
            let Some(choice) = p.selected_choice() else {
                return;
            };
            // Apply the Solo/Crew choice now (independent of which model).
            let crew_mode = p.crew_mode;
            if let Err(e) = crate::cmd::crew::set_crew_enabled(crew_mode) {
                app.push(LineKind::Error, format!("crew mode: {e}"));
            }
            let mode_word = if crew_mode { "crew" } else { "solo" };
            if let Some(name) = choice.configured_name.clone() {
                // Already configured: persist it as the foreman + switch live.
                if let Err(e) = crate::cmd::models::set_user_default(&name) {
                    app.push(LineKind::Error, format!("set default: {e}"));
                }
                if let Some(tx) = ctx.task_tx.as_ref() {
                    let _ = tx.send(AgentCmd::SwitchModel(name.clone()));
                }
                ctx.session.current_model = name.clone();
                app.push(LineKind::Notice, format!("model → {name} ({mode_word})"));
                app.model_picker = None;
                app.mode = ctx.mode_before_overlay.clone();
            } else {
                // New model: open the config form prefilled from defaults.
                app.model_form = Some(ModelForm::from_choice(&choice));
                app.model_picker = None;
                app.mode = Mode::ModelForm;
            }
        }
        KeyCode::Char(c) => {
            p.filter.push(c);
            p.selected = 0;
        }
        _ => {}
    }
}

fn handle_form_key(key: KeyEvent, app: &mut App, ctx: &mut KeyCtx) {
    let Some(form) = app.model_form.as_mut() else {
        app.mode = ctx.mode_before_overlay.clone();
        return;
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            app.model_form = None;
            app.mode = ctx.mode_before_overlay.clone();
        }
        KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % 5,
        KeyCode::BackTab | KeyCode::Up => form.focus = (form.focus + 4) % 5,
        KeyCode::Left if form.focus == 4 => {
            form.reasoning_idx =
                (form.reasoning_idx + REASONING_OPTS.len() - 1) % REASONING_OPTS.len();
        }
        KeyCode::Right if form.focus == 4 => {
            form.reasoning_idx = (form.reasoning_idx + 1) % REASONING_OPTS.len();
        }
        KeyCode::Char('s') if ctrl => save_model_form(app, ctx),
        KeyCode::Enter => {
            // Enter on the last fields saves; otherwise advance.
            if form.focus >= 3 {
                save_model_form(app, ctx);
            } else {
                form.focus += 1;
            }
        }
        KeyCode::Backspace if form.focus < 4 => {
            form.fields[form.focus].pop();
        }
        KeyCode::Char(c) if form.focus < 4 => form.fields[form.focus].push(c),
        _ => {}
    }
}

/// Validate the form, write the model to the user config, and switch to it.
fn save_model_form(app: &mut App, ctx: &mut KeyCtx) {
    let Some(form) = app.model_form.as_mut() else {
        return;
    };
    let name = form.fields[0].trim().to_string();
    if name.is_empty() {
        form.error = Some("name is required".into());
        return;
    }
    let temp: f32 = match form.fields[1].trim().parse() {
        Ok(v) => v,
        Err(_) => {
            form.error = Some("temperature must be a number".into());
            return;
        }
    };
    let context: u32 = match form.fields[2].trim().parse() {
        Ok(v) => v,
        Err(_) => {
            form.error = Some("context window must be an integer".into());
            return;
        }
    };
    let max_output: u32 = match form.fields[3].trim().parse() {
        Ok(v) => v,
        Err(_) => {
            form.error = Some("max output must be an integer".into());
            return;
        }
    };
    let reasoning = form.reasoning().to_string();
    let id = form.id.clone();

    match crate::cmd::models::save_user_model(&name, &id, temp, context, max_output, &reasoning) {
        Ok(()) => {
            if let Some(tx) = ctx.task_tx.as_ref() {
                let _ = tx.send(AgentCmd::SwitchModel(name.clone()));
            }
            if !ctx.session.models.iter().any(|m| m == &name) {
                ctx.session.models.push(name.clone());
            }
            ctx.session.current_model = name.clone();
            app.model_form = None;
            app.mode = ctx.mode_before_overlay.clone();
            app.push(LineKind::Notice, format!("saved & switched → {name}"));
        }
        Err(e) => {
            if let Some(form) = app.model_form.as_mut() {
                form.error = Some(format!("save failed: {e}"));
            }
        }
    }
}

/// The most recent final answer (or agent message) text, for `/copy`.
fn last_answer(app: &App) -> Option<String> {
    app.transcript
        .iter()
        .rev()
        .find(|l| l.kind == LineKind::Final)
        .or_else(|| {
            app.transcript
                .iter()
                .rev()
                .find(|l| l.kind == LineKind::Agent)
        })
        .map(|l| l.text.clone())
}

fn git_diff(root: &Path) -> String {
    super::commands::git_diff(root)
}

/// Append a host report's lines to the transcript.
fn push_report(app: &mut App, report: super::commands::Report) {
    for (error, line) in report.lines {
        app.push(
            if error {
                LineKind::Error
            } else {
                LineKind::Notice
            },
            line,
        );
    }
}

/// Recall the previous message into the input editor.
fn history_recall_prev(app: &mut App, history: &[String], hist_pos: &mut Option<usize>) {
    if history.is_empty() {
        return;
    }
    let pos = hist_pos.unwrap_or(history.len());
    if pos == 0 {
        return;
    }
    let np = pos - 1;
    *hist_pos = Some(np);
    app.set_input(&history[np]);
}

/// Recall the next message (or clear the input past the newest).
fn history_recall_next(app: &mut App, history: &[String], hist_pos: &mut Option<usize>) {
    let Some(pos) = *hist_pos else { return };
    let np = pos + 1;
    if np >= history.len() {
        *hist_pos = None;
        app.set_input("");
    } else {
        *hist_pos = Some(np);
        app.set_input(&history[np]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything `KeyCtx` borrows, owned, so tests can drive the real key/command
    /// handlers and then feed the journal back through the event loop's own path.
    struct Harness {
        prompts: PendingPrompts,
        mode_before_overlay: Mode,
        turn_cancel: TurnCancel,
        task_tx: Option<TaskTx>,
        ui_tx: Sender<UiEvent>,
        history: Vec<String>,
        hist_pos: Option<usize>,
        session: SessionCtx,
        quit_armed: bool,
        echo: LocalEcho,
        sent: Receiver<AgentCmd>,
        _ui_rx: Receiver<UiEvent>,
    }

    impl Harness {
        fn new() -> Self {
            let (tx, sent) = std::sync::mpsc::channel();
            let (ui_tx, _ui_rx) = std::sync::mpsc::channel();
            let echo = LocalEcho::default();
            Self {
                prompts: PendingPrompts::default(),
                mode_before_overlay: Mode::Idle,
                turn_cancel: std::sync::Arc::new(std::sync::Mutex::new(None)),
                task_tx: Some(TaskTx {
                    tx,
                    echo: echo.clone(),
                }),
                ui_tx,
                history: Vec::new(),
                hist_pos: None,
                session: SessionCtx {
                    root: PathBuf::from("/nonexistent"),
                    models: Vec::new(),
                    current_model: String::new(),
                    ranch_id: None,
                    workstream_id: None,
                    suggestions: Vec::new(),
                },
                quit_armed: false,
                echo,
                sent,
                _ui_rx,
            }
        }

        fn ctx(&mut self) -> KeyCtx<'_> {
            KeyCtx {
                prompts: &mut self.prompts,
                mode_before_overlay: &mut self.mode_before_overlay,
                turn_cancel: &self.turn_cancel,
                task_tx: &mut self.task_tx,
                ui_tx: &self.ui_tx,
                history: &mut self.history,
                hist_pos: &mut self.hist_pos,
                session: &mut self.session,
                quit_armed: &mut self.quit_armed,
            }
        }

        fn journal(&mut self, app: &mut App, msg: UiEventMsg) {
            apply_live_wire(app, &mut self.prompts, &self.echo, msg);
        }

        /// The text of the last `Message` sent to the agent.
        fn last_sent_message(&self) -> String {
            let mut last = None;
            while let Ok(cmd) = self.sent.try_recv() {
                if let AgentCmd::Message(m) = cmd {
                    last = Some(m);
                }
            }
            last.expect("no message was sent")
        }
    }

    fn user_lines(app: &App) -> Vec<&str> {
        app.transcript
            .iter()
            .filter(|l| l.kind == LineKind::User)
            .map(|l| l.text.as_str())
            .collect()
    }

    #[test]
    fn journaled_user_messages_render_once_whoever_sent_them() {
        let mut h = Harness::new();
        let mut app = App::new_with_access("t", Access::Interactive);
        send_message(&mut app, &mut h.ctx(), "hello".into());
        h.journal(&mut app, UiEventMsg::UserMessage("hello".into()));
        // Another client's message (the web) has no local echo: the journal is the
        // only place it can come from.
        h.journal(&mut app, UiEventMsg::UserMessage("from the web".into()));
        assert_eq!(user_lines(&app), ["hello", "from the web"]);

        // Replay and observers have no local echo at all.
        for access in [Access::ReadOnlyLive, Access::Replay] {
            let mut app = App::new_with_access("t", access);
            h.journal(&mut app, UiEventMsg::UserMessage("hello".into()));
            assert_eq!(user_lines(&app), ["hello"]);
        }
    }

    /// A slash command echoes what was typed but sends a canned prompt, and it is the
    /// prompt the worker journals. That must neither render a second time nor leave
    /// the echo queue stuck on it.
    #[test]
    fn a_slash_command_renders_its_echo_not_its_canned_prompt() {
        let mut h = Harness::new();
        let mut app = App::new_with_access("t", Access::Interactive);
        assert!(!handle_command("go ship it", &mut app, &mut h.ctx()));
        let prompt = h.last_sent_message();
        assert_ne!(prompt, "/go ship it");
        h.journal(&mut app, UiEventMsg::UserMessage(prompt));
        h.journal(
            &mut app,
            UiEventMsg::UserMessage("later, from the web".into()),
        );
        assert_eq!(user_lines(&app), ["/go ship it", "later, from the web"]);
    }

    #[test]
    fn a_send_that_never_comes_back_does_not_wedge_the_echo() {
        let echo = LocalEcho::default();
        echo.record("lost in transit".into());
        echo.record("delivered".into());
        assert!(echo.take("delivered"));
        // The lost one went with it: a later identical message from elsewhere renders.
        assert!(!echo.take("lost in transit"));
    }

    /// Enter mid-turn steers that turn — one `TurnDone`, not two — so the TUI must go
    /// idle on it rather than waiting for a second that never comes.
    #[test]
    fn steering_mid_turn_goes_idle_on_the_one_turn_done() {
        let mut h = Harness::new();
        let mut app = App::new_with_access("t", Access::Interactive);
        send_message(&mut app, &mut h.ctx(), "start".into());
        assert_eq!(app.mode, Mode::Running);
        h.journal(&mut app, UiEventMsg::UserMessage("start".into()));
        send_message(&mut app, &mut h.ctx(), "also check the error path".into());
        h.journal(
            &mut app,
            UiEventMsg::UserMessage("also check the error path".into()),
        );
        h.journal(&mut app, UiEventMsg::TurnDone);
        assert_eq!(app.mode, Mode::Idle);
        assert_eq!(user_lines(&app).len(), 2);
    }

    #[test]
    fn a_turn_started_by_another_client_runs_the_tui() {
        let mut h = Harness::new();
        let mut app = App::new_with_access("t", Access::Interactive);
        app.mode = Mode::Idle;
        h.journal(
            &mut app,
            UiEventMsg::UserMessage("sent from the web".into()),
        );
        assert_eq!(app.mode, Mode::Running);
        h.journal(&mut app, UiEventMsg::TurnDone);
        assert_eq!(app.mode, Mode::Idle);

        // The published status moves it too (e.g. a Snapshot on attach mid-turn).
        apply_status(&mut app, &mut h.prompts, SessionStatus::Running);
        assert_eq!(app.mode, Mode::Running);
        apply_status(&mut app, &mut h.prompts, SessionStatus::Idle);
        assert_eq!(app.mode, Mode::Idle);
        // ...but never a read-only view.
        let mut ro = App::new_with_access("t", Access::ReadOnlyLive);
        ro.mode = Mode::Idle;
        apply_status(&mut ro, &mut h.prompts, SessionStatus::Running);
        assert_eq!(ro.mode, Mode::Idle);
    }

    /// A queued message runs as its own turn without a fresh `UserMessage` (it was
    /// journaled when queued), so the first `TurnDone` must not idle the view.
    #[test]
    fn a_queued_turn_keeps_the_tui_running_across_turn_done() {
        let mut h = Harness::new();
        let mut app = App::new_with_access("t", Access::Interactive);
        app.mode = Mode::Running;
        h.journal(
            &mut app,
            UiEventMsg::QueueChanged {
                pending: vec!["next".into()],
            },
        );
        h.journal(&mut app, UiEventMsg::TurnDone);
        assert_eq!(app.mode, Mode::Running);
        h.journal(
            &mut app,
            UiEventMsg::QueueChanged {
                pending: Vec::new(),
            },
        );
        h.journal(&mut app, UiEventMsg::TurnDone);
        assert_eq!(app.mode, Mode::Idle);
    }

    #[test]
    fn a_turn_ending_under_an_open_prompt_idles_once_it_is_answered() {
        let mut h = Harness::new();
        let mut app = App::new_with_access("t", Access::Interactive);
        app.mode = Mode::Running;
        let (reply, _answers) = std::sync::mpsc::channel();
        h.prompts.add(
            &mut app,
            UiPrompt::Ask {
                id: 1,
                question: "which?".into(),
                options: Vec::new(),
                reply,
            },
        );
        h.journal(&mut app, UiEventMsg::TurnDone);
        assert!(matches!(app.mode, Mode::AwaitingInput(_)));
        h.prompts.answer_ask(&mut app, "that one".into());
        assert_eq!(app.mode, Mode::Idle);
    }

    #[test]
    fn every_job_state_maps_to_its_own_crew_status() {
        use crate::agent::jobs::JobState;
        let cases = [
            (JobState::Pending, CrewStatus::Pending),
            (JobState::Running, CrewStatus::Running),
            (JobState::AwaitingVerdict { seq: 1 }, CrewStatus::Asking),
            // A job with a question for the foreman is parked, not finished.
            (JobState::AwaitingAnswer { seq: 1 }, CrewStatus::Waiting),
            (JobState::Done { ok: true }, CrewStatus::Done),
            (JobState::Done { ok: false }, CrewStatus::Failed),
        ];
        for (state, want) in cases {
            assert_eq!(crew_status(state.as_str()), want, "{state:?}");
        }
        // A newer worker's state is shown as live, not silently as done.
        assert_eq!(crew_status("some future state"), CrewStatus::Running);
    }

    #[test]
    fn read_only_capabilities_accept_no_mutating_input_and_exit_only_on_q_or_escape() {
        for access in [Access::ReadOnlyLive, Access::Replay] {
            for mode in [
                Mode::Idle,
                Mode::Running,
                Mode::AwaitingInput("question".into()),
                Mode::Done,
            ] {
                assert!(!accepts_paste(access, &mode));
            }
            assert_eq!(read_only_exit_key(access, KeyCode::Char('x')), Some(false));
            assert_eq!(read_only_exit_key(access, KeyCode::Enter), Some(false));
            assert_eq!(read_only_exit_key(access, KeyCode::Char('q')), Some(true));
            assert_eq!(read_only_exit_key(access, KeyCode::Esc), Some(true));
        }
        assert_eq!(
            read_only_exit_key(Access::Interactive, KeyCode::Char('q')),
            None
        );
        assert!(accepts_paste(Access::Interactive, &Mode::Idle));
    }

    #[test]
    fn authoritative_prompts_preserve_matching_channel_and_modal_draft() {
        let mut app = App::new("t");
        app.mode = Mode::Running;
        let mut prompts = PendingPrompts::default();
        let (old_tx, old_rx) = std::sync::mpsc::channel();
        prompts.add(
            &mut app,
            UiPrompt::Ask {
                id: 7,
                question: "continue?".into(),
                options: Vec::new(),
                reply: old_tx,
            },
        );
        app.textarea.insert_str("draft answer");

        let (new_tx, new_rx) = std::sync::mpsc::channel();
        prompts.replace(
            &mut app,
            vec![UiPrompt::Ask {
                id: 7,
                question: "continue?".into(),
                options: Vec::new(),
                reply: new_tx,
            }],
        );

        assert_eq!(app.input_text(), "draft answer");
        assert!(matches!(app.mode, Mode::AwaitingInput(ref q) if q == "continue?"));
        prompts.answer_ask(&mut app, "yes".into());
        assert_eq!(old_rx.recv().unwrap(), "yes");
        assert!(
            new_rx.recv().is_err(),
            "replacement channel must be dropped silently"
        );
        assert_eq!(app.mode, Mode::Running);
    }

    #[test]
    fn resolving_an_exact_prompt_restores_the_next_modal() {
        let mut app = App::new("t");
        app.mode = Mode::Idle;
        let mut prompts = PendingPrompts::default();
        let (ask_tx, _ask_rx) = std::sync::mpsc::channel();
        let (approval_tx, _approval_rx) = tokio::sync::oneshot::channel();
        prompts.replace(
            &mut app,
            vec![
                UiPrompt::Ask {
                    id: 1,
                    question: "first?".into(),
                    options: Vec::new(),
                    reply: ask_tx,
                },
                UiPrompt::Approval {
                    id: 2,
                    dest: "example.com:443".into(),
                    detail: None,
                    reply: approval_tx,
                },
            ],
        );
        assert!(matches!(app.mode, Mode::AwaitingInput(ref q) if q == "first?"));

        prompts.resolve(&mut app, 99);
        assert!(matches!(app.mode, Mode::AwaitingInput(_)));
        prompts.resolve(&mut app, 1);
        assert!(matches!(app.mode, Mode::Approval(ref dest) if dest == "example.com:443"));
        prompts.resolve(&mut app, 2);
        assert_eq!(app.mode, Mode::Idle);
    }

    #[test]
    fn read_only_access_drops_prompts_without_answering() {
        let mut app = App::new_with_access("t", Access::ReadOnlyLive);
        let mut prompts = PendingPrompts::default();
        let (reply, answers) = std::sync::mpsc::channel();
        prompts.add(
            &mut app,
            UiPrompt::Ask {
                id: 1,
                question: "answer?".into(),
                options: Vec::new(),
                reply,
            },
        );
        assert!(answers.recv().is_err());
        assert_eq!(app.mode, Mode::Running);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"hello, cowboy"), "aGVsbG8sIGNvd2JveQ==");
    }

    #[test]
    fn history_recall_walks_messages() {
        let history = vec!["first".to_string(), "second".to_string()];
        let mut app = App::new("t");
        let mut pos = None;

        // Up from a fresh input recalls the newest, then older.
        history_recall_prev(&mut app, &history, &mut pos);
        assert_eq!(app.input_text(), "second");
        history_recall_prev(&mut app, &history, &mut pos);
        assert_eq!(app.input_text(), "first");
        // Can't go past the oldest.
        history_recall_prev(&mut app, &history, &mut pos);
        assert_eq!(app.input_text(), "first");
        // Down walks forward, then clears past the newest.
        history_recall_next(&mut app, &history, &mut pos);
        assert_eq!(app.input_text(), "second");
        history_recall_next(&mut app, &history, &mut pos);
        assert_eq!(app.input_text(), "");
    }

    #[test]
    fn last_answer_prefers_final_then_agent() {
        let mut app = App::new("t");
        assert!(last_answer(&app).is_none());
        app.push(LineKind::Agent, "thinking out loud");
        assert_eq!(last_answer(&app).as_deref(), Some("thinking out loud"));
        app.push(LineKind::Final, "the answer");
        assert_eq!(last_answer(&app).as_deref(), Some("the answer"));
    }

    // Stranded input: the difference between "nothing arrived" and "something
    // arrived that we will never be told about".

    #[test]
    fn an_empty_input_queue_is_never_dropped() {
        // The common case by far: an idle UI polling with nothing to read.
        for polls in [0, 1, 2, 1_000] {
            assert!(!stranded_input_should_drop(0, polls));
        }
    }

    #[test]
    fn one_quiet_poll_is_not_enough_to_call_input_stranded() {
        // Bytes that landed between the poll returning and the queue being read
        // will be delivered on the next iteration — dropping them would eat a
        // keystroke the user legitimately typed.
        assert!(!stranded_input_should_drop(3, 1));
    }

    #[test]
    fn bytes_surviving_two_quiet_polls_are_dropped_to_unfreeze_the_ui() {
        // Two empty polls with the queue still occupied means no readiness edge
        // is coming: without this the loop draws forever and never reads again.
        assert!(stranded_input_should_drop(3, 2));
        assert!(stranded_input_should_drop(2_106, 7));
    }

    #[test]
    fn mouse_tracking_asks_for_drag_motion_but_not_idle_motion() {
        // `?1003h` (any-motion) is what floods the input queue; the selection
        // only needs `?1002h`. Guard the distinction the comment argues for.
        assert!(MOUSE_TRACKING_ON.contains("?1002h"));
        assert!(!MOUSE_TRACKING_ON.contains("?1003h"));
        // SGR encoding is required: without it, columns past 223 can't be reported.
        assert!(MOUSE_TRACKING_ON.contains("?1006h"));
        // Everything turned on gets turned off, plus any-motion defensively.
        for mode in ["1000", "1002", "1006", "1003"] {
            assert!(
                MOUSE_TRACKING_OFF.contains(&format!("?{mode}l")),
                "{mode} left enabled on exit"
            );
        }
    }
}
