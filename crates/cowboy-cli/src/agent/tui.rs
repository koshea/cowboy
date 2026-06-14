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
use cowboy_tui::{draw, App, LineKind, Mode};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use super::ui::AgentUi;

/// Events the agent loop sends to the TUI event loop.
pub enum UiEvent {
    Delta(String),
    ModelDone,
    CommandStart(String),
    CommandEnd(i32, String),
    Final(String),
    Notice(String),
    Ask(String, Sender<String>),
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
                if handle_key(key, &mut app, &mut pending_reply, &cancel) {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Returns true if the loop should exit.
fn handle_key(
    key: KeyEvent,
    app: &mut App,
    pending_reply: &mut Option<Sender<String>>,
    cancel: &CancellationToken,
) -> bool {
    // Ctrl-C interrupts the agent.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        cancel.cancel();
        app.status = "interrupting…".into();
        return false;
    }
    match (&app.mode, key.code) {
        (Mode::Done, KeyCode::Char('q')) | (Mode::Done, KeyCode::Esc) => return true,
        (Mode::AwaitingInput(_), KeyCode::Enter) => {
            let answer = std::mem::take(&mut app.input);
            app.push(LineKind::User, answer.clone());
            if let Some(reply) = pending_reply.take() {
                let _ = reply.send(answer);
            }
            app.mode = Mode::Running;
            app.status = "running".into();
        }
        (_, KeyCode::Char(c)) => app.input.push(c),
        (_, KeyCode::Backspace) => {
            app.input.pop();
        }
        _ => {}
    }
    false
}
