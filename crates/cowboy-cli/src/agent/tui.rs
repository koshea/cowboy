//! ratatui front-end: a [`TuiUi`] adapter implementing [`AgentUi`] plus the
//! terminal event loop.
//!
//! Threading model: the async agent loop runs on a dedicated thread with its
//! own current-thread runtime and holds the `TuiUi`, which forwards display
//! events to the main thread over a channel. `ask_user` blocks the agent
//! thread on a reply channel — safe because it is not a runtime worker shared
//! with anything else.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use cowboy_core::netproto::{ApprovalScope, Verdict};
use cowboy_tui::{draw, App, LineKind, Mode};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use super::ui::AgentUi;

/// A command the TUI sends to the agent thread.
pub enum AgentCmd {
    /// A user message to run as a turn.
    Message(String),
    /// Switch the active model to this name (applies from the next turn).
    SwitchModel(String),
}

/// Events the agent loop / control server send to the TUI event loop.
#[derive(Debug)]
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
#[allow(clippy::too_many_arguments)]
pub fn run_event_loop(
    title: &str,
    intro: Vec<String>,
    seed: Option<String>,
    events: Receiver<UiEvent>,
    task_tx: Sender<AgentCmd>,
    turn_cancel: TurnCancel,
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
        task_tx,
        turn_cancel,
        ctx,
    );
    restore_terminal(&mut terminal)?;
    result
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
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Capture the mouse for scrollback + transcript-scoped text selection (Shift
    // bypasses it for native selection); enable bracketed paste so multi-line
    // pastes arrive as one chunk instead of key-by-key.
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )?;
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
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Copy `text` to the system clipboard via OSC 52. Works in Ghostty, kitty,
/// iTerm2, and over SSH/tmux (with passthrough). Written straight to stdout —
/// it's an escape sequence the terminal consumes, not visible output.
fn clipboard_copy(text: &str) {
    use std::io::Write;
    let seq = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let mut out = io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
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
    task_tx: Sender<AgentCmd>,
    turn_cancel: TurnCancel,
    mut session: SessionCtx,
) -> Result<()> {
    let mut app = App::new(title.to_string());
    let mut pending_reply: Option<Sender<String>> = None;
    let mut pending_approval: Option<tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>> = None;
    let mut mode_before_overlay = Mode::Idle;
    // Outstanding messages sent to the agent but not yet acknowledged (TurnDone).
    let mut pending_turns: usize = 0;
    let mut task_tx = Some(task_tx);
    // Submitted-message history for Up/Down recall.
    let mut history: Vec<String> = Vec::new();
    let mut hist_pos: Option<usize> = None;

    // Welcome banner (project info) at the top of the transcript.
    for line in intro {
        app.push(LineKind::Banner, line);
    }

    // Seed the first turn, or start idle awaiting the first message.
    match seed {
        Some(t) if !t.is_empty() => {
            app.push(LineKind::User, t.clone());
            if let Some(tx) = &task_tx {
                history.push(t.clone());
                let _ = tx.send(AgentCmd::Message(t));
            }
            pending_turns = 1;
            app.mode = Mode::Running;
        }
        _ => app.mode = Mode::Idle,
    }

    loop {
        // If the transcript changes while we're following the tail, the view
        // scrolls and any mouse selection would misalign — drop it.
        let content_sig = (app.transcript.len(), app.streaming.len());
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

        if app.has_selection()
            && app.follow
            && (app.transcript.len(), app.streaming.len()) != content_sig
        {
            app.clear_selection();
        }

        app.tick();
        terminal.draw(|f| draw(f, &app))?;

        if event::poll(Duration::from_millis(120))? {
            match event::read()? {
                // Ignore key *release* events (kitty protocol reports them).
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let ctx = KeyCtx {
                        pending_reply: &mut pending_reply,
                        pending_approval: &mut pending_approval,
                        mode_before_overlay: &mut mode_before_overlay,
                        turn_cancel: &turn_cancel,
                        task_tx: &mut task_tx,
                        pending_turns: &mut pending_turns,
                        history: &mut history,
                        hist_pos: &mut hist_pos,
                        session: &mut session,
                    };
                    if handle_key(Event::Key(key), key, &mut app, ctx) {
                        break;
                    }
                }
                Event::Mouse(me) => handle_mouse(me, &mut app, terminal),
                // Bracketed paste — insert as one chunk (when editing input).
                Event::Paste(text) => {
                    if matches!(
                        app.mode,
                        Mode::Idle | Mode::Running | Mode::AwaitingInput(_)
                    ) {
                        app.input_paste(&text);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Mouse → transcript selection (copy-on-release via OSC 52) + wheel scroll.
fn handle_mouse(me: crossterm::event::MouseEvent, app: &mut App, terminal: &mut Term) {
    use crossterm::event::{MouseButton, MouseEventKind};
    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => app.begin_selection(me.column, me.row),
        MouseEventKind::Drag(MouseButton::Left) => app.drag_selection(me.column, me.row),
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(text) = app.selected_text(terminal.current_buffer_mut()) {
                let chars = text.chars().count();
                clipboard_copy(&text);
                app.status = format!("copied {chars} chars");
            }
        }
        MouseEventKind::ScrollUp => {
            app.clear_selection();
            app.scroll_up(3);
        }
        MouseEventKind::ScrollDown => {
            app.clear_selection();
            app.scroll_down(3);
        }
        _ => {}
    }
}

/// Mutable context handed to the key handler.
struct KeyCtx<'a> {
    pending_reply: &'a mut Option<Sender<String>>,
    pending_approval: &'a mut Option<tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>>,
    mode_before_overlay: &'a mut Mode,
    turn_cancel: &'a TurnCancel,
    /// `None` once the session has been ended (sender dropped).
    task_tx: &'a mut Option<Sender<AgentCmd>>,
    pending_turns: &'a mut usize,
    history: &'a mut Vec<String>,
    hist_pos: &'a mut Option<usize>,
    session: &'a mut SessionCtx,
}

/// Returns true if the loop should exit.
fn handle_key(event: Event, key: KeyEvent, app: &mut App, mut ctx: KeyCtx) -> bool {
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
            if let Some(reply) = ctx.pending_reply.take() {
                let _ = reply.send(answer);
            }
            app.mode = Mode::Running;
            app.status = "running".into();
        }
        // Submit a message (Idle or while a turn is running -> queued).
        (Mode::Idle | Mode::Running, KeyCode::Enter) => {
            let msg = app.take_input();
            let trimmed = msg.trim();
            if trimmed.is_empty() {
                // nothing to do
            } else if let Some(rest) = trimmed.strip_prefix('/') {
                handle_command(rest, app, &mut ctx);
            } else if let Some(tx) = ctx.task_tx {
                app.push(LineKind::User, msg.clone());
                ctx.history.push(msg.clone());
                *ctx.hist_pos = None;
                let _ = tx.send(AgentCmd::Message(msg));
                *ctx.pending_turns += 1;
                app.mode = Mode::Running;
                app.status = "running".into();
            }
        }
        // Everything else is text input for the editor.
        _ => app.input_event(event),
    }
    false
}

/// Help text for `/help`.
const HELP_LINES: &[&str] = &[
    "commands:",
    "  /help          show this help",
    "  /model [name]  show or switch the active model",
    "  /diff          show the working-tree diff",
    "  /copy          copy the last answer to the clipboard",
    "  /clear         clear the view (conversation memory is kept)",
    "  /quit          end the session",
    "keys: Enter send · Shift/Alt+Enter newline · Up/Down history · PgUp/PgDn scroll · Ctrl-C menu",
];

/// Handle a `/command` typed into the input (the leading `/` is stripped).
fn handle_command(input: &str, app: &mut App, ctx: &mut KeyCtx) {
    let mut parts = input.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next();
    match cmd {
        "help" | "h" | "?" => {
            for l in HELP_LINES {
                app.push(LineKind::Notice, *l);
            }
        }
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
            Some(text) => {
                let n = text.chars().count();
                clipboard_copy(&text);
                app.status = format!("copied {n} chars");
            }
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
            Some(name) if !ctx.session.models.iter().any(|m| m == name) => {
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
        "quit" | "exit" | "q" => {
            ctx.task_tx.take();
            if let Some(tok) = ctx.turn_cancel.lock().unwrap().as_ref() {
                tok.cancel();
            }
            app.status = "ending session…".into();
        }
        other => app.push(
            LineKind::Error,
            format!("unknown command /{other} — try /help"),
        ),
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
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("diff")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
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
}
