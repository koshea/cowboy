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
use clipboard_rs::{Clipboard, ClipboardContext};
use cowboy_core::daemonproto::UiEventMsg;
use cowboy_core::netproto::{ApprovalScope, Verdict};
use cowboy_tui::{draw, App, LineKind, Mode, ModelChoice, ModelForm, ModelPicker, REASONING_OPTS};
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
    /// Turn plan mode on/off (file edits are blocked while on).
    PlanMode(bool),
    /// Sign off on this session's ranch workstream (the user typed `/accept`):
    /// complete the workstream, advance the plan, and end the session.
    Accept { note: Option<String> },
    /// Detach this client, leaving the session running for later re-attach.
    Detach,
}

/// Events the agent loop / control server send to the TUI event loop.
///
/// Most events are the journaled display events shared with the daemon wire
/// protocol — they ride inside [`UiEvent::Wire`] rather than being restated
/// here, so the two enums can't drift. The remaining variants are client-only:
/// they carry non-serializable reply channels or are synthesized by the client.
#[derive(Debug)]
pub enum UiEvent {
    /// A journaled display event (the shared [`UiEventMsg`] payload).
    Wire(UiEventMsg),
    /// A question for the user: prompt, suggested options (possibly empty), and
    /// the reply channel.
    Ask(String, Vec<String>, Sender<String>),
    /// A network approval request: destination label + a reply channel.
    Approval(
        String,
        tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>,
    ),
    /// A pending approval was decided elsewhere (another client / timeout);
    /// dismiss the modal if one is showing.
    ApprovalResolved,
    /// The `/models` catalogue finished loading; open the picker.
    ModelsFetched(Vec<ModelChoice>),
    /// The session ended.
    Done,
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
            .send(UiEvent::Ask(question.to_string(), options.to_vec(), rtx))
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
        // The TUI echoes user input locally on submit, so ignore the journaled
        // copy to avoid a double line. (The web client, which can refresh, renders
        // from this instead.)
        UiEventMsg::UserMessage(_) => {}
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
        UiEventMsg::Cost(usd) => app.cost_usd = usd,
        UiEventMsg::Plan(steps) => app.plan = steps,
        UiEventMsg::Blocked(reason) => app.set_blocked(reason),
        UiEventMsg::Title(t) => app.title = t,
        UiEventMsg::Processes(procs) => app.processes = procs,
        UiEventMsg::SubagentStarted { label, model, id } => {
            app.subagent_started(label, model, id, now_ms())
        }
        UiEventMsg::SubagentDone { ok, id, .. } => app.subagent_done(&id, ok),
        // Handled in the event loop (needs loop-local turn bookkeeping).
        UiEventMsg::TurnDone => {}
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
    /// The ranch this session belongs to, if it's a workstream (enables `/accept`).
    pub ranch_id: Option<String>,
    /// The workstream id within the ranch, if any.
    pub workstream_id: Option<String>,
}

type Term = Terminal<CrosstermBackend<Stdout>>;

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

/// Copy `text` to the clipboard via OSC 52 — the *fallback* path, used when the
/// direct OS clipboard (clipboard-rs) is unavailable, e.g. running the TUI over
/// SSH with no local display. Works in Ghostty, kitty, iTerm2, and over
/// SSH/tmux (with passthrough), though some terminal/multiplexer stacks drop it.
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
    // Inside tmux/screen the sequence must be wrapped in DCS passthrough or it
    // never reaches the outer terminal (a very common "copy doesn't work" cause).
    let seq = if std::env::var_os("TMUX").is_some() {
        // tmux: wrap in `\ePtmux;…\e\\` with inner ESCs doubled.
        format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b"))
    } else if std::env::var_os("STY").is_some() {
        // GNU screen: pass through via DCS.
        format!("\x1bP{osc}\x1b\\")
    } else {
        osc
    };
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
    ui_tx: Sender<UiEvent>,
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
    // Slash-command autocomplete catalog (built-ins + discovered skills),
    // computed once: skills rarely change mid-session.
    let completion_catalog = build_completion_catalog(&session);

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

    // Direct OS clipboard handle (X11/Wayland), created once and kept alive for
    // the whole session: on X11 the copied selection is only served while this
    // context lives. `None` when there's no local display (headless / SSH), in
    // which case copies fall back to OSC 52 through the terminal.
    let clipboard = ClipboardContext::new().ok();

    // Byte offset consumed from the watched subagent's journal, reset when the
    // watch target changes, so the nested view tails the file across ticks.
    let mut watch_pos: u64 = 0;
    let mut watch_pos_id = String::new();

    loop {
        while let Ok(ev) = events.try_recv() {
            match ev {
                // TurnDone needs loop-local turn bookkeeping, so it's handled
                // here; every other journaled (wire) event is pure view-state.
                UiEvent::Wire(UiEventMsg::TurnDone) => {
                    pending_turns = pending_turns.saturating_sub(1);
                    app.commit_stream();
                    // Back to idle once all queued turns are processed.
                    if pending_turns == 0 && matches!(app.mode, Mode::Running) {
                        app.mode = Mode::Idle;
                        app.status = "ready".into();
                    }
                }
                UiEvent::Wire(msg) => apply_wire(&mut app, msg),
                UiEvent::Ask(q, options, reply) => {
                    app.commit_stream();
                    if options.is_empty() {
                        app.mode = Mode::AwaitingInput(q);
                    } else {
                        app.begin_choice(q, options);
                    }
                    pending_reply = Some(reply);
                }
                UiEvent::Approval(dest, reply) => {
                    if !matches!(app.mode, Mode::Approval(_) | Mode::Paused) {
                        mode_before_overlay = app.mode.clone();
                    }
                    app.mode = Mode::Approval(dest);
                    pending_approval = Some(reply);
                }
                UiEvent::ApprovalResolved => {
                    // Decided elsewhere: drop our prompt and restore the prior
                    // mode. (If we were the decider we've already moved on.)
                    if matches!(app.mode, Mode::Approval(_)) {
                        pending_approval = None;
                        app.mode = mode_before_overlay.clone();
                    }
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
        terminal.draw(|f| draw(f, &app))?;

        // Flush any queued clipboard copy. Prefer the direct OS clipboard
        // (reliable locally on X11/Wayland); fall back to OSC 52 — written
        // through ratatui's own backend *after* the frame, so it isn't eaten by
        // crossterm's buffered frame — when there's no local display (SSH).
        // Outcome is reported both in the status line and the per-run log
        // ($TMPDIR/cowboy-<pid>.log) so copy failures are diagnosable.
        if let Some(text) = app.take_pending_copy() {
            let n = text.chars().count();
            app.status = match clipboard.as_ref().map(|c| c.set_text(text.clone())) {
                Some(Ok(())) => {
                    eprintln!("[copy] {n} chars -> OS clipboard ok");
                    format!("copied {n} chars")
                }
                Some(Err(e)) => {
                    eprintln!("[copy] OS clipboard failed ({e}); OSC 52 fallback");
                    clipboard_copy(terminal.backend_mut(), &text);
                    format!("copied {n} chars (osc52 fallback)")
                }
                None => {
                    eprintln!("[copy] no OS clipboard (headless/SSH?); OSC 52");
                    clipboard_copy(terminal.backend_mut(), &text);
                    format!("copied {n} chars (osc52)")
                }
            };
        }

        if event::poll(Duration::from_millis(120))? {
            let ev = event::read()?;
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
                        pending_reply: &mut pending_reply,
                        pending_approval: &mut pending_approval,
                        mode_before_overlay: &mut mode_before_overlay,
                        turn_cancel: &turn_cancel,
                        task_tx: &mut task_tx,
                        ui_tx: &ui_tx,
                        pending_turns: &mut pending_turns,
                        history: &mut history,
                        hist_pos: &mut hist_pos,
                        session: &mut session,
                    };
                    if handle_key(Event::Key(key), key, &mut app, ctx) {
                        break;
                    }
                }
                Event::Mouse(me) => handle_mouse(me, &mut app),
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
            // Refresh the slash-command autocomplete when the input changed (so
            // navigation keys, which don't touch the text, keep the selection).
            if matches!(app.mode, Mode::Idle | Mode::Running) {
                if app.input_text() != input_before {
                    refresh_completions(&mut app, &completion_catalog);
                }
            } else {
                app.clear_completions();
            }
        }
    }
    Ok(())
}

/// Built-in slash commands offered by autocomplete (value, hint).
const COMMAND_COMPLETIONS: &[(&str, &str)] = &[
    ("help", "show help"),
    ("skills", "list skills"),
    ("model", "[name] show/switch model"),
    ("models", "browse the model catalogue"),
    (
        "plan",
        "<task> propose a plan first (edits blocked until /go)",
    ),
    ("go", "[note] approve the plan and start editing"),
    (
        "ranch",
        "[note] promote the discussion into a multi-workstream ranch",
    ),
    ("crew", "[usage] show the crew roster"),
    ("mcp", "list connected MCP servers"),
    ("diff", "working-tree diff"),
    ("copy", "copy the last answer"),
    ("clear", "clear the view"),
    ("detach", "leave running, re-attach later"),
    ("quit", "end the session"),
];

/// Build the autocomplete catalog once: built-in commands + discovered skills.
fn build_completion_catalog(session: &SessionCtx) -> Vec<cowboy_tui::Completion> {
    let mut out: Vec<cowboy_tui::Completion> = COMMAND_COMPLETIONS
        .iter()
        .map(|(v, h)| cowboy_tui::Completion {
            value: v.to_string(),
            hint: h.to_string(),
        })
        .collect();
    // `/accept` only makes sense inside a ranch workstream session.
    if session.workstream_id.is_some() {
        out.push(cowboy_tui::Completion {
            value: "accept".to_string(),
            hint: "[note] sign off this workstream and advance the plan".to_string(),
        });
    }
    for s in cowboy_core::skills::discover(&session.root) {
        let hint = s.argument_hint.clone().unwrap_or_else(|| {
            s.description
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        });
        out.push(cowboy_tui::Completion {
            value: s.name,
            hint,
        });
    }
    out
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
    pending_reply: &'a mut Option<Sender<String>>,
    pending_approval: &'a mut Option<tokio::sync::oneshot::Sender<(Verdict, ApprovalScope)>>,
    mode_before_overlay: &'a mut Mode,
    turn_cancel: &'a TurnCancel,
    /// `None` once the session has been ended (sender dropped).
    task_tx: &'a mut Option<Sender<AgentCmd>>,
    /// For posting client-side async results (e.g. the fetched model list).
    ui_tx: &'a Sender<UiEvent>,
    pending_turns: &'a mut usize,
    history: &'a mut Vec<String>,
    hist_pos: &'a mut Option<usize>,
    session: &'a mut SessionCtx,
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
            KeyCode::Char('w') => {
                // Watch a subagent's live output (cycles if several). No-op with none.
                match app.next_watch_target() {
                    Some((id, label)) => app.watch_subagent(id, label),
                    None => {
                        app.status = "no subagents to watch".into();
                        app.mode = ctx.mode_before_overlay.clone();
                    }
                }
            }
            KeyCode::Char('d') => {
                // Detach: leave the session running and exit this client.
                if let Some(tx) = ctx.task_tx.as_ref() {
                    let _ = tx.send(AgentCmd::Detach);
                }
                app.status = "detaching…".into();
                return true; // exit the event loop; the worker keeps running
            }
            KeyCode::Char('e') => {
                // End the session: drop the task sender so the agent finalizes.
                // Dropping it hangs up the bridge's command channel, which sends
                // `End` to the worker; the worker finalizes and broadcasts
                // `Ended`, which arrives as `UiEvent::Done`. Close the overlay so
                // the user sees the transcript + "ending session…" while that
                // round-trip happens (otherwise the menu lingers and it looks like
                // the key did nothing).
                ctx.task_tx.take();
                if let Some(tok) = ctx.turn_cancel.lock().unwrap().as_ref() {
                    tok.cancel();
                }
                app.mode = ctx.mode_before_overlay.clone();
                app.status = "ending session…".into();
            }
            _ => {}
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
            if let Some(reply) = ctx.pending_reply.take() {
                let _ = reply.send(answer);
            }
            app.mode = Mode::Running;
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
                app.choice = None;
                app.push(LineKind::User, answer.clone());
                if let Some(reply) = ctx.pending_reply.take() {
                    let _ = reply.send(answer);
                }
                app.mode = Mode::Running;
                app.status = "running".into();
            }
        }
        (Mode::AwaitingChoice, KeyCode::Enter) => {
            let answer = app.choice_answer();
            app.push(LineKind::User, answer.clone());
            if let Some(reply) = ctx.pending_reply.take() {
                let _ = reply.send(answer);
            }
            app.mode = Mode::Running;
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
    "  /skills        list available skills",
    "  /<skill> [args]  run a skill (e.g. /github:review-pr 162)",
    "  /plan <task>   propose a plan first — edits are blocked until you approve",
    "  /go [note]     approve the plan and let the agent start editing",
    "  /ranch [note]  promote the discussion into a multi-workstream ranch plan",
    "  /accept [note] sign off this ranch workstream → advance the plan (workstreams only)",
    "  /model [name]  show or switch the active model",
    "  /models        browse the provider catalogue and add/select a model",
    "  /crew [usage]  show the crew roster (model routing) or its usage",
    "  /mcp           list connected MCP servers (manage with `cowboy mcp`)",
    "  /diff          show the working-tree diff",
    "  /copy          copy the last answer to the system clipboard",
    "  /clear         clear the view (conversation memory is kept)",
    "  /detach        leave the session running and exit (re-attach later)",
    "  /quit          end the session",
    "copy: drag to select (drag to the top/bottom edge to extend across scrollback),",
    "      then `y` to copy (Esc clears) · or /copy for the whole last answer",
    "keys: Enter send · Shift/Alt+Enter newline · Up/Down history · Ctrl-C menu",
    "scroll: PgUp/PgDn · Shift+Up/Down line · Shift+End jump to tail & follow",
    "Ctrl-C menu: r resume · i instruct (redirect) · k kill turn · d detach · e end",
];

/// `/mcp`: list the configured MCP servers (host + this repo's trust-gated
/// `.mcp.json`) as notices. Read-only — manage servers with the `cowboy mcp` CLI.
fn mcp_command(app: &mut App, root: &std::path::Path) {
    let cfg = match cowboy_core::mcp::load_or_default() {
        Ok(cfg) => cfg,
        Err(e) => {
            app.push(LineKind::Error, format!("MCP config error: {e}"));
            return;
        }
    };
    if cfg.servers.is_empty() {
        app.push(LineKind::Notice, "no host MCP servers configured");
    } else {
        app.push(LineKind::Notice, "MCP servers (host):");
        for (name, s) in &cfg.servers {
            let state = if s.enabled { "enabled" } else { "disabled" };
            let desc = if s.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", s.description)
            };
            app.push(
                LineKind::Notice,
                format!("  {name} [{state}] {}{desc}", s.transport_label()),
            );
        }
    }
    // This repo's `.mcp.json`, if any (trust-gated).
    let state = crate::mcp::trust::project_trust(root);
    if state != crate::mcp::trust::TrustState::NoFile {
        if let Ok(Some(servers)) = cowboy_core::mcp::load_project_mcp(root) {
            app.push(
                LineKind::Notice,
                format!("MCP servers (.mcp.json) — {}:", state.label()),
            );
            for (name, s) in &servers {
                app.push(
                    LineKind::Notice,
                    format!("  {name} {}", s.transport_label()),
                );
            }
            if matches!(
                state,
                crate::mcp::trust::TrustState::Untrusted | crate::mcp::trust::TrustState::Stale
            ) {
                app.push(LineKind::Notice, "  → enable with `cowboy mcp trust`");
            }
        }
    }
    app.push(
        LineKind::Notice,
        "manage with `cowboy mcp add/trust/remove/test`",
    );
}

/// `/crew` (and `/crew usage`): show the crew roster matrix or usage summary as
/// notices. Read-only — manage the roster with the `cowboy crew` CLI.
fn crew_command(arg: Option<&str>, app: &mut App) {
    use cowboy_core::crew;
    if arg == Some("usage") {
        let rows = crew::usage_by_model(&crew::load_history());
        if rows.is_empty() {
            app.push(LineKind::Notice, "no recorded crew activity yet");
            return;
        }
        app.push(LineKind::Notice, "crew usage (per model):");
        for r in rows {
            app.push(
                LineKind::Notice,
                format!(
                    "  {:<14} {} tasks · {}% ok · {}ms avg",
                    r.model,
                    r.tasks,
                    r.success_pct(),
                    r.avg_duration_ms()
                ),
            );
        }
        return;
    }
    match crew::load() {
        Ok(Some(cfg)) => {
            // Shorten ids to their last path segment so the grid stays readable.
            let short = |m: &str| m.rsplit('/').next().unwrap_or(m).to_string();
            let foreman =
                crate::cmd::crew::foreman_model().unwrap_or_else(|| "<default>".to_string());
            let mut col_w = crew::Effort::all()
                .iter()
                .map(|e| e.as_str().len())
                .max()
                .unwrap_or(6);
            for cat in cfg.crew.keys() {
                for (_, model) in cfg.expanded(cat, &foreman) {
                    col_w = col_w.max(short(&model).len());
                }
            }
            col_w += 2;
            app.push(
                LineKind::Notice,
                format!(
                    "crew foreman: {}   delegation: {}",
                    foreman,
                    if cfg.enabled() { "on" } else { "off (solo)" }
                ),
            );
            let mut header = format!("{:<14}", "CATEGORY");
            for e in crew::Effort::all() {
                header.push_str(&format!("{:<col_w$}", e.as_str()));
            }
            app.push(LineKind::Notice, header);
            for cat in cfg.crew.keys() {
                let mut row = format!("{cat:<14}");
                for (_, model) in cfg.expanded(cat, &foreman) {
                    row.push_str(&format!("{:<col_w$}", short(&model)));
                }
                app.push(LineKind::Notice, row);
            }
            app.push(
                LineKind::Notice,
                "(edit with the `cowboy crew` CLI; `/crew usage` for activity)",
            );
        }
        Ok(None) => app.push(
            LineKind::Notice,
            "no crew roster — create one with `cowboy crew init`",
        ),
        Err(e) => app.push(LineKind::Error, format!("crew: {e}")),
    }
}

/// Handle a `/command` typed into the input (the leading `/` is stripped).
/// Handle a `/command`. Returns `true` if the client should exit the event loop
/// now (e.g. `/detach`).
fn handle_command(input: &str, app: &mut App, ctx: &mut KeyCtx) -> bool {
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
                let prompt = format!(
                    "Plan mode is ON. Research the codebase READ-ONLY (read/grep/ls — do not \
                     modify files or run state-changing commands), then present a concise, \
                     numbered plan of the steps you'll take. Use the `plan` tool to list the \
                     steps. Then stop and wait — I'll review and run /go to approve.\n\nTask: {task}"
                );
                app.push(LineKind::User, format!("/{input}"));
                let _ = tx.send(AgentCmd::Message(prompt));
                *ctx.pending_turns += 1;
                app.mode = Mode::Running;
                app.status = "planning…".into();
            }
        }
        "go" => {
            let note = input.strip_prefix("go").unwrap_or("").trim();
            if let Some(tx) = ctx.task_tx.as_ref() {
                let _ = tx.send(AgentCmd::PlanMode(false));
                app.plan_mode = false;
                let extra = if note.is_empty() {
                    String::new()
                } else {
                    format!(" Also: {note}")
                };
                app.push(LineKind::User, format!("/{input}"));
                let _ = tx.send(AgentCmd::Message(format!(
                    "Approved — implement the plan now.{extra}"
                )));
                *ctx.pending_turns += 1;
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
                let extra = if note.is_empty() {
                    String::new()
                } else {
                    format!(" Emphasis: {note}.")
                };
                app.push(LineKind::User, format!("/{input}"));
                let _ = tx.send(AgentCmd::Message(format!(
                    "This is bigger than one session — promote it into a multi-workstream Ranch \
                     Plan. Using what we've already discussed (don't re-research from scratch), \
                     decompose the work into independent, parallelizable workstreams wired by \
                     dependencies. Write the decomposition to `.cowboy/ranch-plan.yaml` with the \
                     `write` tool (a YAML doc with `title`, `goal`, and a `workstreams` list — each \
                     with `id`, `goal`, optional `title`, `depends_on`, `expected_artifacts`, \
                     `acceptance`), then run `cowboy ranch draft .cowboy/ranch-plan.yaml` to \
                     validate and draft it. Do not implement anything.{extra}"
                )));
                *ctx.pending_turns += 1;
                app.mode = Mode::Running;
                app.status = "drafting a ranch…".into();
            }
        }
        "crew" => crew_command(arg, app),
        "mcp" => mcp_command(app, &ctx.session.root),
        "quit" | "exit" | "q" => {
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
                let mut body = skill.instructions.clone();
                if body.contains("$ARGUMENTS") {
                    body = body.replace("$ARGUMENTS", args);
                } else if !args.is_empty() {
                    body.push_str(&format!("\n\nArguments: {args}"));
                }
                let prompt = format!("Run the `{}` skill.\n\n{body}", skill.name);
                if let Some(tx) = ctx.task_tx.as_ref() {
                    app.push(LineKind::User, format!("/{input}"));
                    let _ = tx.send(AgentCmd::Message(prompt));
                    *ctx.pending_turns += 1;
                    app.mode = Mode::Running;
                    app.status = format!("running skill {}", skill.name);
                }
            } else {
                app.push(
                    LineKind::Error,
                    format!("unknown command /{other} — try /help or /skills"),
                );
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
