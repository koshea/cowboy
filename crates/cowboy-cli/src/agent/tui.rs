//! ratatui front-end: a [`TuiUi`] adapter implementing [`AgentUi`] plus the
//! terminal event loop.
//!
//! Threading model: the async agent loop runs on a dedicated thread with its
//! own current-thread runtime and holds the `TuiUi`, which forwards display
//! events to the main thread over a channel. `ask_user` blocks the agent
//! thread on a reply channel — safe because it is not a runtime worker shared
//! with anything else.

use std::io::{self, Stdout};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use cowboy_core::netproto::{ApprovalScope, Verdict};
use cowboy_tui::{draw, App, LineKind, Mode};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use super::ui::AgentUi;

/// Events the agent loop / control server send to the TUI event loop.
pub enum UiEvent {
    Delta(String),
    ModelDone,
    CommandStart(String),
    CommandOutput(String),
    CommandEnd(i32, String),
    ToolUse(String),
    Final(String),
    Notice(String),
    Ask(String, Sender<String>),
    /// A network approval request: destination label + a reply channel.
    Approval(
        String,
        tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>,
    ),
    /// A network decision the gateway made, for the activity log.
    NetEvent(String),
    /// Working-tree diff summary for the status bar.
    DiffStat(String),
    /// Running session token estimate (input, output).
    Tokens(u64, u64),
    /// Update the transcript title (cwd + branch context).
    Title(String),
    /// Managed processes (name, status) for the process pane.
    Processes(Vec<(String, String)>),
    /// The agent finished a turn; ready for the next user message.
    TurnDone,
    Done,
}

/// `AgentUi` implementation that forwards to the TUI thread.
pub struct TuiUi {
    pub tx: Sender<UiEvent>,
}

impl AgentUi for TuiUi {
    fn model_delta(&mut self, text: &str) {
        let _ = self.tx.send(UiEvent::Delta(text.to_string()));
    }
    fn model_done(&mut self) {
        let _ = self.tx.send(UiEvent::ModelDone);
    }
    fn command_start(&mut self, command: &str) {
        let _ = self.tx.send(UiEvent::CommandStart(command.to_string()));
    }
    fn command_output(&mut self, chunk: &str) {
        let _ = self.tx.send(UiEvent::CommandOutput(chunk.to_string()));
    }
    fn command_end(&mut self, exit_code: i32, output: &str) {
        let _ = self
            .tx
            .send(UiEvent::CommandEnd(exit_code, output.to_string()));
    }
    fn tool_use(&mut self, summary: &str) {
        let _ = self.tx.send(UiEvent::ToolUse(summary.to_string()));
    }
    fn tokens(&mut self, input: u64, output: u64) {
        let _ = self.tx.send(UiEvent::Tokens(input, output));
    }
    fn final_message(&mut self, message: &str) {
        let _ = self.tx.send(UiEvent::Final(message.to_string()));
    }
    fn ask_user(&mut self, question: &str) -> String {
        let (rtx, rrx) = std::sync::mpsc::channel();
        if self
            .tx
            .send(UiEvent::Ask(question.to_string(), rtx))
            .is_err()
        {
            return String::new();
        }
        rrx.recv().unwrap_or_default()
    }
    fn notice(&mut self, msg: &str) {
        let _ = self.tx.send(UiEvent::Notice(msg.to_string()));
    }
}

/// Shared handle to the current turn's cancellation token (set by the agent
/// thread, fired by the TUI's interrupt menu).
pub type TurnCancel = std::sync::Arc<std::sync::Mutex<Option<CancellationToken>>>;

/// Run the conversational TUI event loop. `intro` lines are shown as a welcome
/// banner; `seed` is an optional first message; `task_tx` sends the user's
/// messages to the agent thread (dropping it ends the session); `turn_cancel`
/// interrupts the in-flight turn.
pub fn run_event_loop(
    title: &str,
    intro: Vec<String>,
    seed: Option<String>,
    events: Receiver<UiEvent>,
    task_tx: Sender<String>,
    turn_cancel: TurnCancel,
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
        task_tx,
        turn_cancel,
    );
    restore_terminal(&mut terminal)?;
    result
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
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

fn event_loop(
    terminal: &mut Term,
    title: &str,
    intro: Vec<String>,
    seed: Option<String>,
    events: Receiver<UiEvent>,
    task_tx: Sender<String>,
    turn_cancel: TurnCancel,
) -> Result<()> {
    let mut app = App::new(title.to_string());
    let mut pending_reply: Option<Sender<String>> = None;
    let mut pending_approval: Option<tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>> = None;
    let mut mode_before_overlay = Mode::Idle;
    // Outstanding messages sent to the agent but not yet acknowledged (TurnDone).
    let mut pending_turns: usize = 0;
    let mut task_tx = Some(task_tx);

    // Welcome banner (project info) at the top of the transcript.
    for line in intro {
        app.push(LineKind::Banner, line);
    }

    // Seed the first turn, or start idle awaiting the first message.
    match seed {
        Some(t) if !t.is_empty() => {
            app.push(LineKind::User, t.clone());
            if let Some(tx) = &task_tx {
                let _ = tx.send(t);
            }
            pending_turns = 1;
            app.mode = Mode::Running;
        }
        _ => app.mode = Mode::Idle,
    }

    loop {
        while let Ok(ev) = events.try_recv() {
            match ev {
                UiEvent::Delta(t) => app.stream(&t),
                UiEvent::ModelDone => app.commit_stream(),
                UiEvent::CommandStart(c) => {
                    app.commit_stream();
                    app.push(LineKind::Command, c.clone());
                    app.status = format!("exec: {c}");
                }
                UiEvent::CommandOutput(chunk) => {
                    // Streamed live, one line at a time.
                    app.push(LineKind::Output, chunk.trim_end_matches('\n'));
                }
                UiEvent::CommandEnd(code, _out) => {
                    if code != 0 {
                        app.push(LineKind::Error, format!("[exit {code}]"));
                    }
                    app.status = "running".into();
                }
                UiEvent::ToolUse(s) => {
                    app.commit_stream();
                    app.push(LineKind::Tool, s);
                }
                UiEvent::Final(m) => {
                    // `final` ends the turn, not the session.
                    app.commit_stream();
                    app.push(LineKind::Final, m);
                }
                UiEvent::Notice(m) => app.push(LineKind::Notice, m),
                UiEvent::Ask(q, reply) => {
                    app.commit_stream();
                    app.mode = Mode::AwaitingInput(q);
                    pending_reply = Some(reply);
                }
                UiEvent::Approval(dest, reply) => {
                    if !matches!(app.mode, Mode::Approval(_) | Mode::Paused) {
                        mode_before_overlay = app.mode.clone();
                    }
                    app.mode = Mode::Approval(dest);
                    pending_approval = Some(reply);
                }
                UiEvent::NetEvent(line) => app.activity(line),
                UiEvent::DiffStat(s) => app.diff = s,
                UiEvent::Tokens(i, o) => {
                    app.tokens_in = i;
                    app.tokens_out = o;
                }
                UiEvent::Title(t) => app.title = t,
                UiEvent::Processes(procs) => app.processes = procs,
                UiEvent::TurnDone => {
                    pending_turns = pending_turns.saturating_sub(1);
                    app.commit_stream();
                    // Back to idle once all queued turns are processed.
                    if pending_turns == 0 && matches!(app.mode, Mode::Running) {
                        app.mode = Mode::Idle;
                        app.status = "ready".into();
                    }
                }
                UiEvent::Done => {
                    app.mode = Mode::Done;
                    app.status = "session ended".into();
                }
            }
        }

        app.tick();
        terminal.draw(|f| draw(f, &app))?;

        if event::poll(Duration::from_millis(120))? {
            match event::read()? {
                Event::Key(key) => {
                    let ctx = KeyCtx {
                        pending_reply: &mut pending_reply,
                        pending_approval: &mut pending_approval,
                        mode_before_overlay: &mut mode_before_overlay,
                        turn_cancel: &turn_cancel,
                        task_tx: &mut task_tx,
                        pending_turns: &mut pending_turns,
                    };
                    if handle_key(Event::Key(key), key, &mut app, ctx) {
                        break;
                    }
                }
                Event::Mouse(me) => match me.kind {
                    crossterm::event::MouseEventKind::ScrollUp => app.scroll_up(3),
                    crossterm::event::MouseEventKind::ScrollDown => app.scroll_down(3),
                    _ => {}
                },
                _ => {}
            }
        }
    }
    Ok(())
}

/// Mutable context handed to the key handler.
struct KeyCtx<'a> {
    pending_reply: &'a mut Option<Sender<String>>,
    pending_approval: &'a mut Option<tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>>,
    mode_before_overlay: &'a mut Mode,
    turn_cancel: &'a TurnCancel,
    /// `None` once the session has been ended (sender dropped).
    task_tx: &'a mut Option<Sender<String>>,
    pending_turns: &'a mut usize,
}

/// Returns true if the loop should exit.
fn handle_key(event: Event, key: KeyEvent, app: &mut App, ctx: KeyCtx) -> bool {
    // Transcript scrollback — works in any mode so you can read while the agent
    // runs or while typing. Uses keys the text editor doesn't claim.
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::PageUp => {
            app.scroll_up(10);
            return false;
        }
        KeyCode::PageDown => {
            app.scroll_down(10);
            return false;
        }
        KeyCode::Up if shift => {
            app.scroll_up(1);
            return false;
        }
        KeyCode::Down if shift => {
            app.scroll_down(1);
            return false;
        }
        KeyCode::End if shift => {
            app.scroll_to_bottom();
            return false;
        }
        _ => {}
    }

    // Ctrl-C opens the interrupt menu (during a turn or while idle).
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if matches!(
            app.mode,
            Mode::Running | Mode::Idle | Mode::AwaitingInput(_)
        ) {
            *ctx.mode_before_overlay = app.mode.clone();
            app.mode = Mode::Paused;
        }
        return false;
    }

    // Network approval modal.
    if let Mode::Approval(_) = &app.mode {
        let decision = match key.code {
            KeyCode::Char('o') => Some((Verdict::Allow, ApprovalScope::Once)),
            KeyCode::Char('s') => Some((Verdict::Allow, ApprovalScope::Session)),
            KeyCode::Char('p') => Some((Verdict::Allow, ApprovalScope::Project)),
            KeyCode::Char('g') => Some((Verdict::Allow, ApprovalScope::Global)),
            KeyCode::Char('d') | KeyCode::Esc => Some((Verdict::Deny, ApprovalScope::Once)),
            _ => None,
        };
        if let Some(d) = decision {
            if let Some(reply) = ctx.pending_approval.take() {
                let _ = reply.send(d);
            }
            app.mode = ctx.mode_before_overlay.clone();
        }
        return false;
    }

    // Interrupt menu: resume / instruct / kill (this turn) / end (session).
    if app.mode == Mode::Paused {
        match key.code {
            KeyCode::Char('r') | KeyCode::Esc => {
                app.mode = ctx.mode_before_overlay.clone();
            }
            KeyCode::Char('i') => {
                // Instruct: cancel the current turn and drop to Idle with the
                // input focused. The user's next message starts a fresh turn
                // with the full conversation (container + history) intact.
                if let Some(tok) = ctx.turn_cancel.lock().unwrap().as_ref() {
                    tok.cancel();
                }
                app.status = "give a new instruction…".into();
                app.mode = Mode::Idle;
            }
            KeyCode::Char('k') => {
                // Cancel just the current turn; the session continues.
                if let Some(tok) = ctx.turn_cancel.lock().unwrap().as_ref() {
                    tok.cancel();
                }
                app.status = "interrupting turn…".into();
                app.mode = ctx.mode_before_overlay.clone();
            }
            KeyCode::Char('e') => {
                // End the session: drop the task sender so the agent finalizes.
                ctx.task_tx.take();
                if let Some(tok) = ctx.turn_cancel.lock().unwrap().as_ref() {
                    tok.cancel();
                }
                app.status = "ending session…".into();
            }
            _ => {}
        }
        return false;
    }

    match (&app.mode, key.code) {
        (Mode::Done, _) => return true,
        (Mode::AwaitingInput(_), KeyCode::Enter) => {
            let answer = app.take_input();
            app.push(LineKind::User, answer.clone());
            if let Some(reply) = ctx.pending_reply.take() {
                let _ = reply.send(answer);
            }
            app.mode = Mode::Running;
            app.status = "running".into();
        }
        // Submit a message (Idle or while a turn is running -> queued).
        (Mode::Idle | Mode::Running, KeyCode::Enter) => {
            let msg = app.take_input();
            if !msg.trim().is_empty() {
                if let Some(tx) = ctx.task_tx {
                    app.push(LineKind::User, msg.clone());
                    let _ = tx.send(msg);
                    *ctx.pending_turns += 1;
                    app.mode = Mode::Running;
                    app.status = "running".into();
                }
            }
        }
        // Everything else is text input for the editor.
        _ => app.input_event(event),
    }
    false
}
