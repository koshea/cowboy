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
    CommandEnd(i32, String),
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
    fn command_end(&mut self, exit_code: i32, output: &str) {
        let _ = self
            .tx
            .send(UiEvent::CommandEnd(exit_code, output.to_string()));
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

/// Run the TUI event loop until the agent finishes and the user quits.
///
/// `events` receives display events from the agent thread; `cancel` is fired on
/// Ctrl-C to interrupt the agent.
pub fn run_event_loop(
    title: &str,
    user_task: &str,
    events: Receiver<UiEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, title, user_task, events, cancel);
    restore_terminal(&mut terminal)?;
    result
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop(
    terminal: &mut Term,
    title: &str,
    user_task: &str,
    events: Receiver<UiEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut app = App::new(title.to_string());
    app.push(LineKind::User, user_task);
    let mut pending_reply: Option<Sender<String>> = None;
    let mut pending_approval: Option<tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>> = None;
    // Mode to restore after an overlay (approval/paused) is dismissed.
    let mut mode_before_overlay = Mode::Running;

    loop {
        // Drain agent events.
        while let Ok(ev) = events.try_recv() {
            match ev {
                UiEvent::Delta(t) => app.stream(&t),
                UiEvent::ModelDone => app.commit_stream(),
                UiEvent::CommandStart(c) => {
                    app.commit_stream();
                    app.push(LineKind::Command, c.clone());
                    app.status = format!("exec: {c}");
                }
                UiEvent::CommandEnd(code, out) => {
                    app.push(LineKind::Output, out);
                    if code != 0 {
                        app.push(LineKind::Error, format!("[exit {code}]"));
                    }
                    app.status = "running".into();
                }
                UiEvent::Final(m) => {
                    app.commit_stream();
                    app.push(LineKind::Final, m);
                    app.mode = Mode::Done;
                    app.status = "finished".into();
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
                UiEvent::Done => {
                    if app.mode != Mode::Done {
                        app.mode = Mode::Done;
                        app.status = "finished".into();
                    }
                }
            }
        }

        app.tick();
        terminal.draw(|f| draw(f, &app))?;

        // Handle input with a short poll so the spinner animates.
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(key) = event::read()? {
                let ctx = KeyCtx {
                    pending_reply: &mut pending_reply,
                    pending_approval: &mut pending_approval,
                    mode_before_overlay: &mut mode_before_overlay,
                    cancel: &cancel,
                };
                if handle_key(Event::Key(key), key, &mut app, ctx) {
                    break;
                }
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
    cancel: &'a CancellationToken,
}

/// Returns true if the loop should exit. `event` is the full crossterm event
/// (fed to the input editor); `key` is its key form (for shortcuts).
fn handle_key(event: Event, key: KeyEvent, app: &mut App, ctx: KeyCtx) -> bool {
    // Ctrl-C opens the interrupt menu (unless one is already open / done).
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if matches!(app.mode, Mode::Running | Mode::AwaitingInput(_)) {
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

    // Interrupt menu: resume / instruct / kill / end.
    if app.mode == Mode::Paused {
        match key.code {
            KeyCode::Char('r') | KeyCode::Esc => app.mode = ctx.mode_before_overlay.clone(),
            KeyCode::Char('e') | KeyCode::Char('k') => {
                ctx.cancel.cancel();
                app.status = "interrupting…".into();
                app.mode = ctx.mode_before_overlay.clone();
            }
            KeyCode::Char('i') => {
                app.activity("instruct: type in the input box, then resume");
                app.mode = ctx.mode_before_overlay.clone();
            }
            _ => {}
        }
        return false;
    }

    match (&app.mode, key.code) {
        (Mode::Done, KeyCode::Char('q')) | (Mode::Done, KeyCode::Esc) => return true,
        (Mode::AwaitingInput(_), KeyCode::Enter) => {
            let answer = app.take_input();
            app.push(LineKind::User, answer.clone());
            if let Some(reply) = ctx.pending_reply.take() {
                let _ = reply.send(answer);
            }
            app.mode = Mode::Running;
            app.status = "running".into();
        }
        // Everything else is text input for the editor.
        _ => app.input_event(event),
    }
    false
}
