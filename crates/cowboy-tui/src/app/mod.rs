//! Renderable TUI state and drawing. The CLI owns the event loop and feeds
//! this `App`; here we keep state + a pure `draw` so rendering is
//! snapshot-testable with `ratatui::backend::TestBackend`.

use crate::markdown;
use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use ratatui_textarea::TextArea;
use throbber_widgets_tui::{Throbber, ThrobberState};

mod render;
pub use render::draw;
use render::{transcript_lines, LinkHit};

/// Kind of a transcript line (drives color/prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Welcome / project-info banner shown at startup.
    Banner,
    User,
    Agent,
    Command,
    /// A structured tool action (read/edit/write).
    Tool,
    Output,
    /// A unified diff of a file the agent created or edited (rendered with
    /// +/- coloring).
    Diff,
    Final,
    Notice,
    Error,
}

/// One line in the conversation transcript.
#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
}

/// Interaction mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Agent is working on a turn.
    Running,
    /// Agent finished its turn; ready for the next user message.
    Idle,
    AwaitingInput(String),
    /// Answering a multiple-choice question (state in `App::choice`).
    AwaitingChoice,
    Approval(String),
    /// Browsing the keys/commands reference (state in `App::help`).
    ///
    /// Replaced `Paused`. That mode was a modal you had to open with Ctrl-C and then
    /// pick a letter from before anything happened — including interrupting, which is
    /// the one thing you press Ctrl-C in a hurry to do. Its eight options are now direct
    /// hotkeys, and this overlay is where you look them up rather than where you invoke
    /// them.
    Help,
    /// Choosing a model from the provider catalogue (state in `App::model_picker`).
    ModelPicker,
    /// Configuring a newly chosen model (state in `App::model_form`).
    ModelForm,
    /// Watching a subagent's live output (the watched session in `App::watching`).
    WatchingSubagent,
    Done,
}

/// Reasoning-effort choices offered in the model form (index order).
pub const REASONING_OPTS: [&str; 5] = ["none", "minimal", "low", "medium", "high"];

/// One model offered in the `/models` picker. Carries both the existing config
/// (if any) and the shipped defaults used to prefill a new model's form.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    /// Provider-side id, e.g. `cerebras/zai-glm-4.7`.
    pub id: String,
    /// Display label (configured friendly name, else the suggested name).
    pub label: String,
    pub configured: bool,
    pub current: bool,
    /// The existing config key, if this model is already configured.
    pub configured_name: Option<String>,
    // Prefill for a new model's form:
    pub suggested_name: String,
    pub context_window: u32,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Reasoning effort label ("low"/"medium"/"high"/"minimal"), or None.
    pub reasoning: Option<String>,
}

/// Scrollable model-catalogue picker state.
#[derive(Debug, Clone, Default)]
pub struct ModelPicker {
    pub entries: Vec<ModelChoice>,
    pub filter: String,
    pub selected: usize,
    /// Whether the chosen model runs as a crew foreman (delegates) or solo.
    /// Toggled with Tab; applied on selection.
    pub crew_mode: bool,
}

impl ModelPicker {
    /// Entries matching the current filter (case-insensitive over id + label).
    pub fn filtered(&self) -> Vec<&ModelChoice> {
        let f = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                f.is_empty()
                    || e.id.to_lowercase().contains(&f)
                    || e.label.to_lowercase().contains(&f)
            })
            .collect()
    }
    pub fn selected_choice(&self) -> Option<ModelChoice> {
        self.filtered().get(self.selected).map(|c| (*c).clone())
    }
    pub fn move_sel(&mut self, delta: isize) {
        let n = self.filtered().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = (cur + delta).rem_euclid(n as isize) as usize;
    }
    /// Keep the selection valid after the filter changes.
    pub fn clamp(&mut self) {
        let n = self.filtered().len();
        if self.selected >= n {
            self.selected = n.saturating_sub(1);
        }
    }
}

/// Editable form for a newly chosen model. Text fields are indices 0..=3
/// (name, temperature, context window, max output); focus 4 is reasoning effort.
#[derive(Debug, Clone, Default)]
pub struct ModelForm {
    pub id: String,
    /// [name, temperature, context_window, max_output] as edited text.
    pub fields: [String; 4],
    /// Index into [`REASONING_OPTS`].
    pub reasoning_idx: usize,
    /// Focused field: 0..=3 text fields, 4 = reasoning effort.
    pub focus: usize,
    pub error: Option<String>,
}

impl ModelForm {
    pub const FIELD_LABELS: [&'static str; 4] =
        ["Name", "Temperature", "Context window", "Max output"];

    /// Build a form prefilled from a picker choice's defaults.
    pub fn from_choice(c: &ModelChoice) -> Self {
        let reasoning_idx = REASONING_OPTS
            .iter()
            .position(|r| Some(*r) == c.reasoning.as_deref())
            .unwrap_or(0);
        ModelForm {
            id: c.id.clone(),
            fields: [
                c.suggested_name.clone(),
                format!("{}", c.temperature),
                c.context_window.to_string(),
                c.max_tokens.to_string(),
            ],
            reasoning_idx,
            focus: 0,
            error: None,
        }
    }
    pub fn reasoning(&self) -> &'static str {
        REASONING_OPTS[self.reasoning_idx]
    }
}

/// A pending multiple-choice question (the `ask_user` pick-list).
#[derive(Debug, Clone, Default)]
pub struct Choice {
    pub question: String,
    pub options: Vec<String>,
    /// Highlighted option index.
    pub selected: usize,
}

/// Full renderable TUI state.
/// One autocomplete candidate (a slash command or a skill).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The token inserted after `/` (e.g. `help`, `github:review-pr`).
    pub value: String,
    /// A short usage/description hint shown beside it.
    pub hint: String,
}

/// Active slash-command autocomplete popup state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionState {
    pub items: Vec<Completion>,
    pub selected: usize,
}

/// One group of the help overlay: a heading and its `key/command → meaning` rows.
///
/// Built by the CLI, not here, because what belongs in it is context-dependent — the
/// slash-command table, the skills discovered in this project, whether this session is a
/// ranch workstream. This crate only knows how to draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpSection {
    pub title: String,
    pub rows: Vec<(String, String)>,
}

/// The help overlay: grouped rows plus where the reader has scrolled to.
///
/// Scrollable because the content does not fit. The old `/help` printed 25 lines into the
/// transcript, which pushed the conversation off-screen to tell you how to use it, and a
/// fixed-size modal would simply truncate on a short terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelpView {
    pub sections: Vec<HelpSection>,
    /// First visible content row.
    pub scroll: usize,
    /// Rows the last render could show, so paging knows the page size.
    pub viewport: std::cell::Cell<usize>,
    /// Rows the last render produced. Measured rather than computed because
    /// descriptions wrap, so the count depends on the width — computing it here would
    /// clamp scrolling short of the tail on a narrow terminal.
    pub total: std::cell::Cell<usize>,
    /// Set when the overlay was opened with a filter (`/help jobs`), for the title.
    pub filter: Option<String>,
}

impl HelpView {
    /// Scroll by `delta` rows, clamped so the last page stays full rather than scrolling
    /// into empty space. Both bounds come from the last render, so calling this before
    /// one has happened is a no-op rather than a jump to a wrong offset.
    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.total.get().saturating_sub(self.viewport.get().max(1));
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, max as isize) as usize;
    }
}

/// One aggregated network-activity row: a verdict + destination, with how many
/// times it's been seen (gateway decisions repeat a lot for chatty hosts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub verdict: String,
    pub host: String,
    pub count: u32,
}

/// Parse a raw gateway-activity line (`"<verdict> <host:port> (<reason>)"`) into
/// its verdict and destination, dropping the verbose reason.
fn parse_activity(raw: &str) -> (String, String) {
    let raw = raw.trim();
    let (verdict, rest) = raw.split_once(' ').unwrap_or((raw, ""));
    let host = rest
        .split_once(" (")
        .map(|(h, _)| h)
        .unwrap_or(rest)
        .trim()
        .to_string();
    (verdict.to_string(), host)
}

/// The shell command currently running, for the live status indicator.
#[derive(Debug, Clone)]
pub struct RunningCmd {
    pub cmd: String,
    /// Wall-clock start (ms since epoch), set by the event loop.
    pub started_ms: u64,
    /// Seconds elapsed, refreshed by the event loop each tick.
    pub elapsed_secs: u64,
    /// Most recent output line (the live tail).
    pub last: String,
}

/// Status of a spawned subagent (crew member).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrewStatus {
    /// Planned but waiting for a concurrency permit (per-provider cap) — not yet
    /// consuming a model connection.
    Pending,
    Running,
    /// Spent its turn grant and is waiting for the foreman to answer. Distinct from
    /// `Running` because nothing is happening: the worker is parked, and the reason it
    /// is parked is a decision someone has to make.
    Asking,
    Done,
    Failed,
}

/// A spawned crew subagent, shown in the background pane.
#[derive(Debug, Clone)]
pub struct CrewMember {
    /// The subagent's session id — used to open its live journal when watching.
    pub id: String,
    /// Short routing label (e.g. category or agent name).
    pub label: String,
    /// Resolved model (displayed shortened to its last path segment).
    pub model: String,
    pub status: CrewStatus,
    pub started_ms: u64,
    /// Seconds elapsed (frozen when the subagent finishes).
    pub elapsed_secs: u64,
    /// Turns spent / granted, and the host ceiling. Zero when the roster runs without
    /// turn supervision.
    pub used: u32,
    pub granted: u32,
    pub ceiling: u32,
    /// When `Asking`: how many more turns it wants.
    pub requested: u32,
}

/// What the live prompt costs, as last reported by the agent loop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextSnapshot {
    pub used: u64,
    pub budget: u64,
    pub window: u64,
    pub reserve: u64,
    pub top: Vec<(String, u64)>,
}

impl ContextSnapshot {
    /// Percentage of the conversation budget in use, saturating at the budget being
    /// zero (a window too small for the reserve — reported as full, which it is).
    pub fn percent(&self) -> u64 {
        if self.budget == 0 {
            return 100;
        }
        (self.used * 100 / self.budget).min(999)
    }
}

pub struct App {
    pub title: String,
    pub status: String,
    /// Working-tree diff summary for the status bar (e.g. `Δ 2 files +30 -4`).
    pub diff: String,
    /// Running session token estimate (input/prompt, output/completion).
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Latest context-window snapshot: (used, budget, window, reserve) plus the
    /// biggest consumers. `None` until the first request of the session goes out.
    pub context: Option<ContextSnapshot>,
    /// Running estimated session spend in USD (0.0 when pricing is unknown).
    pub cost_usd: f64,
    pub transcript: Vec<TranscriptLine>,
    /// In-progress streamed agent text (not yet committed to the transcript).
    pub streaming: String,
    /// In-progress streamed "thinking" (reasoning), shown dimmed and cleared
    /// when the response commits. Never added to the transcript.
    pub reasoning: String,
    /// Network activity log (gateway decisions), aggregated by verdict+host so
    /// repeated hits collapse to one row with a count.
    pub activity: Vec<ActivityEntry>,
    /// Managed processes: (name, status). Shown in the background pane.
    pub processes: Vec<(String, String)>,
    /// Spawned crew subagents (this turn's fan-out), shown in the background pane.
    pub crew: Vec<CrewMember>,
    /// Input the user has queued to run *after* the current turn, oldest first. Shown
    /// in the status bar so deferred work is visible rather than invisible until it
    /// suddenly starts.
    pub queued: Vec<String>,
    /// The agent's working plan: ordered (step, status) pairs. When non-empty a
    /// dedicated pane is shown on the right.
    pub plan: Vec<(String, String)>,
    /// Set while the session has declared itself blocked (shown in the status bar).
    pub blocked: Option<String>,
    /// The shell command currently executing, for the live "running" indicator.
    pub running: Option<RunningCmd>,
    /// True when the last transcript line is a transient (carriage-return)
    /// progress update, so the next output chunk overwrites it in place.
    pub last_output_transient: bool,
    /// Number of visible output lines for the running command (its live tail in
    /// the transcript). Bounded to `CMD_TAIL_LINES`.
    cmd_out_visible: usize,
    /// Output lines collapsed away from the running command's tail (shown as a
    /// "⋯ N earlier lines hidden" marker just above the tail).
    cmd_out_hidden: usize,
    /// Whether the running command's output block has a "hidden lines" marker.
    cmd_out_marker: bool,
    /// Input editor (multi-line, cursor) via ratatui-textarea.
    pub textarea: TextArea<'static>,
    pub mode: Mode,
    pub throbber: ThrobberState,
    /// Plan mode is engaged (set by `/plan`, cleared by `/go`): the agent is
    /// proposing a plan and file edits are blocked. Surfaced persistently in the
    /// status bar so you always know edits are gated.
    pub plan_mode: bool,
    /// Wall-clock start of the current model turn (ms since epoch) and the
    /// seconds elapsed, so a thinking turn shows a live "thinking Ns" heartbeat
    /// instead of a silent spinner. Managed by `tick_turn`.
    turn_started_ms: Option<u64>,
    turn_elapsed_secs: u64,
    /// When true, the transcript follows the tail (newest output). When false,
    /// it stays anchored at `scroll_top` so new output doesn't move the view.
    pub follow: bool,
    /// Absolute wrapped-line offset from the top, used while not following.
    pub scroll_top: usize,
    /// Max scroll offset, updated each frame from the rendered content/viewport.
    pub max_scroll: std::cell::Cell<usize>,
    /// Active mouse text-selection in the transcript (absolute screen coords).
    pub selection: Option<Selection>,
    /// True between mouse-down and mouse-up while drag-selecting. Lets us treat
    /// `Moved` events as drags (some terminals report button-held motion as
    /// `Moved` rather than `Drag`).
    pub selecting: bool,
    /// Wrapped-line offset of the top visible row, captured each frame so the
    /// event loop can convert screen rows to logical (wrapped-line) positions.
    pub scroll_offset: std::cell::Cell<usize>,
    /// Whether we were following the tail when the current selection began, so
    /// finishing the selection (copy/clear) can resume following.
    followed_before_select: bool,
    /// Inner text rect of the transcript, captured each frame so the event loop
    /// can hit-test mouse coordinates against the transcript only.
    pub transcript_area: std::cell::Cell<Rect>,
    /// Bumped whenever the transcript/streaming/reasoning content changes, so the
    /// per-frame line build can be memoized: re-parsing Markdown + re-running
    /// syntax highlighting on every throbber tick (when nothing changed) would be
    /// wasteful. The cache holds the version it was built at + the built lines.
    content_ver: std::cell::Cell<u64>,
    line_cache: std::cell::RefCell<Option<(u64, Vec<Line<'static>>)>>,
    /// OSC 8 hyperlink placements (absolute wrapped row + column span + URL),
    /// computed by rendering the transcript off-screen and locating link cells.
    /// Keyed by (content version, width) so it's reused across frames/scroll.
    link_cache: std::cell::RefCell<Option<(u64, u16, Vec<LinkHit>)>>,
    /// Model-catalogue picker state (set while `mode == ModelPicker`).
    pub model_picker: Option<ModelPicker>,
    /// New-model config form state (set while `mode == ModelForm`).
    pub model_form: Option<ModelForm>,
    /// Pending multiple-choice question (set while `mode == AwaitingChoice`).
    pub choice: Option<Choice>,
    /// Keys/commands reference (set while `mode == Help`).
    pub help: Option<HelpView>,
    /// Slash-command autocomplete popup (set while the input is `/<partial>`).
    pub completion: Option<CompletionState>,
    /// Text awaiting copy to the system clipboard. The event loop drains it
    /// *after* the frame is flushed, writing the OSC 52 sequence through
    /// ratatui's own backend — writing to an independent stdout handle here
    /// races crossterm's buffered frame and the bytes get eaten.
    pub pending_copy: Option<String>,
    /// When watching a subagent (`mode == WatchingSubagent`): a nested `App` fed by
    /// that subagent's journal, rendered in place of the main transcript. Boxed to
    /// break the recursive type. `watch_id` is the watched subagent's session id.
    pub watching: Option<Box<App>>,
    pub watch_id: String,
    pub watch_label: String,
}

/// A text selection in *logical* transcript coordinates: a wrapped-line index
/// (0-based from the top of the wrapped content) and a column within the
/// transcript's inner width. Anchoring to logical lines (rather than screen
/// rows) means the selection survives scrolling, so it can span the whole
/// scrollback, not just what's currently on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag began: (wrapped-line index, column).
    pub anchor: (usize, u16),
    /// Current drag position: (wrapped-line index, column).
    pub cursor: (usize, u16),
}

impl Selection {
    /// Endpoints ordered so the selection reads top-to-bottom, left-to-right.
    fn ordered(&self) -> ((usize, u16), (usize, u16)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Inclusive column range `[x0, x1]` selected on wrapped line `line`, given
    /// the inner width, or `None` if the line is outside the selection.
    fn cols_on(&self, line: usize, inner_w: u16) -> Option<(u16, u16)> {
        let (start, end) = self.ordered();
        if line < start.0 || line > end.0 {
            return None;
        }
        let last = inner_w.saturating_sub(1);
        let x0 = if line == start.0 { start.1 } else { 0 };
        let x1 = if line == end.0 { end.1 } else { last };
        (x1 >= x0).then(|| (x0.min(last), x1.min(last)))
    }
}

impl App {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            status: "ready".into(),
            diff: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            context: None,
            cost_usd: 0.0,
            transcript: Vec::new(),
            streaming: String::new(),
            reasoning: String::new(),
            activity: Vec::new(),
            processes: Vec::new(),
            crew: Vec::new(),
            queued: Vec::new(),
            plan: Vec::new(),
            blocked: None,
            running: None,
            last_output_transient: false,
            cmd_out_visible: 0,
            cmd_out_hidden: 0,
            cmd_out_marker: false,
            textarea: TextArea::default(),
            mode: Mode::Running,
            throbber: ThrobberState::default(),
            plan_mode: false,
            turn_started_ms: None,
            turn_elapsed_secs: 0,
            follow: true,
            scroll_top: 0,
            max_scroll: std::cell::Cell::new(0),
            selection: None,
            selecting: false,
            scroll_offset: std::cell::Cell::new(0),
            followed_before_select: false,
            transcript_area: std::cell::Cell::new(Rect::ZERO),
            content_ver: std::cell::Cell::new(0),
            line_cache: std::cell::RefCell::new(None),
            link_cache: std::cell::RefCell::new(None),
            model_picker: None,
            model_form: None,
            choice: None,
            help: None,
            completion: None,
            pending_copy: None,
            watching: None,
            watch_id: String::new(),
            watch_label: String::new(),
        }
    }

    /// Enter watch mode for a subagent: a fresh nested `App` (driven by that
    /// subagent's journal) replaces the main transcript until `stop_watching`.
    pub fn watch_subagent(&mut self, id: impl Into<String>, label: impl Into<String>) {
        self.watch_label = label.into();
        self.watch_id = id.into();
        let mut sub = Box::new(App::new(self.watch_label.clone()));
        sub.mode = Mode::Running;
        self.watching = Some(sub);
        self.mode = Mode::WatchingSubagent;
    }

    /// Leave watch mode, returning to the main session view.
    pub fn stop_watching(&mut self) {
        self.watching = None;
        self.watch_id.clear();
        self.mode = Mode::Idle;
    }

    /// Open the help overlay over whatever the current mode is.
    ///
    /// The caller stashes the mode to come back to; the overlay never changes what the
    /// agent is doing, so it can be opened mid-turn and dismissed with no side effect.
    pub fn open_help(&mut self, sections: Vec<HelpSection>, filter: Option<String>) {
        self.help = Some(HelpView {
            sections,
            scroll: 0,
            viewport: std::cell::Cell::new(1),
            total: std::cell::Cell::new(0),
            filter,
        });
        self.mode = Mode::Help;
    }

    /// Close the help overlay, returning to `back_to`.
    pub fn close_help(&mut self, back_to: Mode) {
        self.help = None;
        self.mode = back_to;
    }

    /// Pick the next subagent to watch (cycles through the crew, most-recent
    /// first); `None` if there are no subagents. Used to toggle/cycle the view.
    pub fn next_watch_target(&self) -> Option<(String, String)> {
        if self.crew.is_empty() {
            return None;
        }
        // Order shown in the pane is insertion order; cycle in that order.
        let cur = self.crew.iter().position(|m| m.id == self.watch_id);
        let next = match cur {
            Some(i) => (i + 1) % self.crew.len(),
            None => 0,
        };
        let m = &self.crew[next];
        Some((m.id.clone(), m.label.clone()))
    }

    /// Request that `text` be copied to the system clipboard. The actual OSC 52
    /// write is deferred to the event loop (see [`App::pending_copy`]).
    pub fn request_copy(&mut self, text: impl Into<String>) {
        self.pending_copy = Some(text.into());
    }

    /// Take any text queued for clipboard copy (called by the event loop).
    pub fn take_pending_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    /// Set the autocomplete candidates (selection reset to the top); empty clears.
    pub fn set_completions(&mut self, items: Vec<Completion>) {
        self.completion = if items.is_empty() {
            None
        } else {
            Some(CompletionState { items, selected: 0 })
        };
    }

    pub fn clear_completions(&mut self) {
        self.completion = None;
    }

    pub fn has_completions(&self) -> bool {
        self.completion.is_some()
    }

    /// Move the autocomplete selection, wrapping.
    pub fn completion_move(&mut self, delta: isize) {
        if let Some(c) = &mut self.completion {
            let n = c.items.len() as isize;
            if n > 0 {
                c.selected = (c.selected as isize + delta).rem_euclid(n) as usize;
            }
        }
    }

    /// Replace the input with the selected completion (`/<value> `) and dismiss.
    pub fn accept_completion(&mut self) {
        if let Some(c) = &self.completion {
            if let Some(item) = c.items.get(c.selected) {
                let v = item.value.clone();
                self.set_input(&format!("/{v} "));
            }
        }
        self.completion = None;
    }

    /// Map an absolute screen position to a logical (wrapped-line, column)
    /// position within the transcript, if it lands inside the transcript rect.
    fn screen_to_logical(&self, col: u16, row: u16) -> Option<(usize, u16)> {
        let r = self.transcript_area.get();
        if !r.contains(Position::new(col, row)) {
            return None;
        }
        let line = self.scroll_offset.get() + (row - r.y) as usize;
        Some((line, col - r.x))
    }

    /// Begin a selection at an absolute screen position, but only if it lands in
    /// the transcript (clicks elsewhere just clear any selection).
    pub fn begin_selection(&mut self, col: u16, row: u16) {
        self.followed_before_select = self.follow;
        if let Some(pos) = self.screen_to_logical(col, row) {
            self.selection = Some(Selection {
                anchor: pos,
                cursor: pos,
            });
            self.selecting = true;
        } else {
            self.selection = None;
            self.selecting = false;
        }
    }

    /// End the drag (mouse-up); the selection itself is kept for `y` to copy.
    pub fn end_selecting(&mut self) {
        self.selecting = false;
    }

    /// Extend the active selection toward an absolute screen position. Dragging
    /// to (or past) the top/bottom edge auto-scrolls so the selection can grow
    /// beyond the visible region, across the scrollback.
    pub fn drag_selection(&mut self, col: u16, row: u16) {
        let r = self.transcript_area.get();
        if self.selection.is_none() || r.height == 0 {
            return;
        }
        // Pin the view while dragging: detach from the tail so streaming output
        // doesn't slide the content out from under the selection. (Shift+End
        // resumes following afterward.)
        if self.follow {
            self.follow = false;
            self.scroll_top = self.scroll_offset.get();
        }
        // Auto-scroll when dragging at the edges — adjust scroll_top directly
        // (not via scroll_down, which would re-engage follow mid-selection).
        if row <= r.y {
            self.scroll_top = self.scroll_top.saturating_sub(1);
        } else if row >= r.bottom().saturating_sub(1) {
            self.scroll_top = (self.scroll_top + 1).min(self.max_scroll.get());
        }
        let rr = row.clamp(r.y, r.bottom().saturating_sub(1));
        let cc = col.clamp(r.x, r.right().saturating_sub(1)) - r.x;
        let line = self.scroll_offset.get() + (rr - r.y) as usize;
        if let Some(sel) = &mut self.selection {
            sel.cursor = (line, cc);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selecting = false;
    }

    /// Finish a selection (after copy or Esc): clear it and, if we were
    /// following the tail when it began, resume following — so a quick
    /// select-and-copy doesn't strand you in scrollback while output streams.
    pub fn finish_selection(&mut self) {
        self.clear_selection();
        if self.followed_before_select {
            self.follow = true;
        }
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// Extract the selected text by rendering the transcript off-screen and
    /// reading the selected logical (wrapped-line) range. Works across the whole
    /// scrollback, not just the visible viewport. `None` for a bare click
    /// (anchor == cursor) or an all-whitespace selection.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let (start, end) = sel.ordered();
        if start == end {
            return None; // a click, not a drag
        }
        let inner_w = self.transcript_area.get().width;
        if inner_w == 0 {
            return None;
        }
        // Render the transcript (no block) into a buffer covering just the
        // selected wrapped-line range: scroll past the lines above `start`, then
        // render `height` rows. Uses ratatui's own word-wrapper, so the wrapping
        // matches what's on screen exactly.
        let height = (end.0 - start.0 + 1).min(u16::MAX as usize) as u16;
        let para = Paragraph::new(transcript_lines(self))
            .wrap(Wrap { trim: false })
            .scroll((start.0.min(u16::MAX as usize) as u16, 0));
        let mut buf = Buffer::empty(Rect::new(0, 0, inner_w, height));
        para.render(buf.area, &mut buf);

        let mut out: Vec<String> = Vec::with_capacity(height as usize);
        for i in 0..height {
            let line = start.0 + i as usize;
            let Some((x0, x1)) = sel.cols_on(line, inner_w) else {
                out.push(String::new());
                continue;
            };
            let mut s = String::new();
            for x in x0..=x1 {
                s.push_str(buf[(x, i)].symbol());
            }
            out.push(s.trim_end().to_string());
        }
        let text = out.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    /// Scroll the transcript up (toward older content) by `n` lines.
    pub fn scroll_up(&mut self, n: usize) {
        if self.follow {
            // Detach from the tail at the current bottom position.
            self.follow = false;
            self.scroll_top = self.max_scroll.get();
        }
        self.scroll_top = self.scroll_top.saturating_sub(n);
    }

    /// Scroll the transcript down (toward the tail) by `n` lines.
    pub fn scroll_down(&mut self, n: usize) {
        if self.follow {
            return;
        }
        self.scroll_top += n;
        if self.scroll_top >= self.max_scroll.get() {
            self.follow = true;
        }
    }

    /// Jump back to following the tail.
    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
    }

    /// True when the view is pinned to the latest output.
    pub fn at_bottom(&self) -> bool {
        self.follow
    }

    /// Invalidate the memoized transcript-line cache (content changed).
    fn touch(&self) {
        self.content_ver.set(self.content_ver.get().wrapping_add(1));
    }

    pub fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.touch();
        // Any non-output line ends a transient progress run (the next output
        // chunk must append, not overwrite this line).
        if kind != LineKind::Output {
            self.last_output_transient = false;
        }
        self.transcript.push(TranscriptLine {
            kind,
            text: text.into(),
        });
        // Bound the scrollback: every frame re-wraps and re-parses the whole
        // transcript, so an unbounded buffer (e.g. a noisy command) tanks
        // performance. Keep the most recent `TRANSCRIPT_CAP` lines, trimming in
        // batches (drop only when we exceed the cap by a slack) so trimming is
        // amortized O(1) rather than O(n) on every push.
        const TRANSCRIPT_CAP: usize = 5000;
        const SLACK: usize = 512;
        if self.transcript.len() > TRANSCRIPT_CAP + SLACK {
            let excess = self.transcript.len() - TRANSCRIPT_CAP;
            self.transcript.drain(0..excess);
        }
    }

    /// Mark the start of a streamed shell command (for the live indicator).
    pub fn start_command(&mut self, cmd: impl Into<String>, now_ms: u64) {
        self.last_output_transient = false;
        self.cmd_out_visible = 0;
        self.cmd_out_hidden = 0;
        self.cmd_out_marker = false;
        self.running = Some(RunningCmd {
            cmd: cmd.into(),
            started_ms: now_ms,
            elapsed_secs: 0,
            last: String::new(),
        });
    }

    /// A one-line-per-job summary of the background pane, for `/jobs` and the pause
    /// menu. Read from the pane the client already maintains, so it needs no round
    /// trip to the worker — the point is to answer "what is still running?" while the
    /// agent is busy.
    pub fn jobs_summary(&self) -> String {
        if self.crew.is_empty() {
            return "no background subagents in this session".to_string();
        }
        let mut s = String::from("background subagents:");
        for m in &self.crew {
            let state = match m.status {
                CrewStatus::Pending => "queued",
                CrewStatus::Running => "running",
                CrewStatus::Asking => "waiting for your decision",
                CrewStatus::Done => "done",
                CrewStatus::Failed => "failed",
            };
            let model = m.model.rsplit('/').next().unwrap_or(&m.model);
            s.push_str(&format!(
                "\n  {} [{}] {model} — {state} · {}s",
                m.id, m.label, m.elapsed_secs
            ));
            if m.granted > 0 {
                s.push_str(&format!(" · turns {}/{}", m.used, m.granted));
            }
            if m.status == CrewStatus::Asking {
                s.push_str(&format!(" · asking for {} more", m.requested));
            }
        }
        let live = self
            .crew
            .iter()
            .filter(|m| {
                matches!(
                    m.status,
                    CrewStatus::Running | CrewStatus::Pending | CrewStatus::Asking
                )
            })
            .count();
        if live > 0 {
            s.push_str(&format!(
                "\n{live} still running — Ctrl-C then `s` stops them, `w` watches one"
            ));
        }
        s
    }

    /// Reconcile the background pane from a level-triggered snapshot.
    ///
    /// Preferred over the `subagent_*` edges where available: those cannot express a
    /// worker parked waiting for a decision, or how much of its turn grant it has
    /// spent. Takes already-mapped rows, keeping this crate free of the wire protocol.
    pub fn apply_jobs(&mut self, members: Vec<CrewMember>) {
        self.crew = members;
    }

    /// How many background jobs are not finished, for the status bar.
    pub fn live_jobs(&self) -> usize {
        self.crew
            .iter()
            .filter(|m| {
                matches!(
                    m.status,
                    CrewStatus::Running | CrewStatus::Pending | CrewStatus::Asking
                )
            })
            .count()
    }

    /// Refresh the running command's elapsed time (called each event-loop tick).
    pub fn tick_command(&mut self, now_ms: u64) {
        if let Some(r) = &mut self.running {
            r.elapsed_secs = now_ms.saturating_sub(r.started_ms) / 1000;
        }
    }

    /// Clear the running-command indicator (command finished).
    pub fn end_command(&mut self) {
        self.running = None;
        self.last_output_transient = false;
    }

    /// A subagent was planned but is waiting for a concurrency permit. Shows as
    /// *pending* in the pane until `subagent_started` flips it to running. A fresh
    /// fan-out (no member still pending or running) replaces the previous batch.
    pub fn subagent_pending(
        &mut self,
        label: impl Into<String>,
        model: impl Into<String>,
        id: impl Into<String>,
        now_ms: u64,
    ) {
        if !self
            .crew
            .iter()
            .any(|m| matches!(m.status, CrewStatus::Running | CrewStatus::Pending))
        {
            self.crew.clear();
        }
        self.crew.push(CrewMember {
            id: id.into(),
            label: label.into(),
            model: model.into(),
            status: CrewStatus::Pending,
            started_ms: now_ms,
            elapsed_secs: 0,
            used: 0,
            granted: 0,
            ceiling: 0,
            requested: 0,
        });
    }

    /// A subagent acquired its permit and is now running. Flips its existing
    /// pending entry (matched by id) to running and (re)starts its clock; falls
    /// back to inserting one if no pending entry exists (e.g. throttle disabled,
    /// or an older worker that never emitted `SubagentPending`).
    pub fn subagent_started(
        &mut self,
        label: impl Into<String>,
        model: impl Into<String>,
        id: impl Into<String>,
        now_ms: u64,
    ) {
        let id = id.into();
        if let Some(m) = self
            .crew
            .iter_mut()
            .find(|m| m.id == id && m.status == CrewStatus::Pending)
        {
            m.status = CrewStatus::Running;
            m.started_ms = now_ms; // clock the run, not the queue wait
            return;
        }
        if !self
            .crew
            .iter()
            .any(|m| matches!(m.status, CrewStatus::Running | CrewStatus::Pending))
        {
            self.crew.clear();
        }
        self.crew.push(CrewMember {
            id,
            label: label.into(),
            model: model.into(),
            status: CrewStatus::Running,
            started_ms: now_ms,
            elapsed_secs: 0,
            used: 0,
            granted: 0,
            ceiling: 0,
            requested: 0,
        });
    }

    /// A subagent finished (matched by its session id; freezes its elapsed time).
    pub fn subagent_done(&mut self, id: &str, ok: bool) {
        if let Some(m) = self
            .crew
            .iter_mut()
            .find(|m| m.id == id && matches!(m.status, CrewStatus::Running | CrewStatus::Pending))
        {
            m.status = if ok {
                CrewStatus::Done
            } else {
                CrewStatus::Failed
            };
        }
    }

    /// Refresh elapsed time for running crew members (event-loop tick).
    pub fn tick_crew(&mut self, now_ms: u64) {
        for m in &mut self.crew {
            if m.status == CrewStatus::Running {
                m.elapsed_secs = now_ms.saturating_sub(m.started_ms) / 1000;
            }
        }
    }

    /// Freeze every still-running crew member's timer. Called when the session
    /// ends/exits: the worker dies before emitting `SubagentDone` for in-flight
    /// subagents, so without this their elapsed time would tick forever in the
    /// (now dead) background pane.
    pub fn freeze_crew(&mut self) {
        for m in &mut self.crew {
            if matches!(m.status, CrewStatus::Running | CrewStatus::Pending) {
                m.status = CrewStatus::Failed;
            }
        }
    }

    /// Track how long the current model turn has been running, so a thinking
    /// turn shows a live "thinking Ns" instead of a silent spinner. Self-managing
    /// off the mode: starts the clock when Running, resets otherwise.
    pub fn tick_turn(&mut self, now_ms: u64) {
        if self.mode == Mode::Running {
            let start = *self.turn_started_ms.get_or_insert(now_ms);
            self.turn_elapsed_secs = now_ms.saturating_sub(start) / 1000;
        } else {
            self.turn_started_ms = None;
            self.turn_elapsed_secs = 0;
        }
    }

    /// Seconds the current model turn has been running (0 when not Running).
    pub fn turn_elapsed_secs(&self) -> u64 {
        self.turn_elapsed_secs
    }

    /// Append (or, for a transient carriage-return update, overwrite-in-place) a
    /// line of streamed command output, and update the live tail.
    ///
    /// A command's visible output is bounded to `CMD_TAIL_LINES`: once it grows
    /// past that, the oldest lines are collapsed into a single "⋯ N earlier lines
    /// hidden" marker above the tail. A verbose command (a `mise install`, a
    /// gcloud stack trace) shows a live tail instead of scrolling the whole pane
    /// away — the full output still reaches the model and the session log.
    pub fn command_output_line(&mut self, text: impl Into<String>, committed: bool) {
        /// Live-tail size for a single running command's output.
        const CMD_TAIL_LINES: usize = 12;
        self.touch();
        let text = text.into();
        let replace = self.last_output_transient
            && self
                .transcript
                .last()
                .is_some_and(|l| l.kind == LineKind::Output);
        if replace {
            // In-place overwrite of a transient progress line — count unchanged.
            if let Some(last) = self.transcript.last_mut() {
                last.text = text.clone();
            }
        } else {
            self.push(LineKind::Output, text.clone());
            self.cmd_out_visible += 1;
            // Collapse the oldest line(s) of this command's block into a marker
            // so the tail stays bounded. Output streams contiguously, so the
            // command's visible lines are the trailing `cmd_out_visible` entries
            // (with the marker, if any, immediately above them).
            if self.running.is_some() && self.cmd_out_visible > CMD_TAIL_LINES {
                let oldest = self.transcript.len() - self.cmd_out_visible;
                self.transcript.remove(oldest);
                self.cmd_out_visible -= 1;
                self.cmd_out_hidden += 1;
                let marker = format!(
                    "⋯ {} earlier line{} hidden",
                    self.cmd_out_hidden,
                    if self.cmd_out_hidden == 1 { "" } else { "s" }
                );
                let run_start = self.transcript.len() - self.cmd_out_visible;
                if self.cmd_out_marker {
                    self.transcript[run_start - 1].text = marker;
                } else {
                    self.transcript.insert(
                        run_start,
                        TranscriptLine {
                            kind: LineKind::Notice,
                            text: marker,
                        },
                    );
                    self.cmd_out_marker = true;
                }
            }
        }
        self.last_output_transient = !committed;
        if let Some(r) = &mut self.running {
            r.last = text;
        }
    }

    /// Record a network-activity line, aggregating by verdict+host: a repeat
    /// bumps the existing row's count instead of appending a duplicate. Caps the
    /// number of distinct rows (oldest dropped) so the pane stays bounded.
    pub fn activity(&mut self, line: impl Into<String>) {
        const MAX_ROWS: usize = 100;
        let (verdict, host) = parse_activity(&line.into());
        if let Some(e) = self
            .activity
            .iter_mut()
            .find(|e| e.verdict == verdict && e.host == host)
        {
            e.count = e.count.saturating_add(1);
            return;
        }
        if self.activity.len() >= MAX_ROWS {
            self.activity.remove(0);
        }
        self.activity.push(ActivityEntry {
            verdict,
            host,
            count: 1,
        });
    }

    /// Record a blocked/unblocked transition (status-bar flag + a notice line).
    pub fn set_blocked(&mut self, reason: Option<String>) {
        match &reason {
            Some(r) => self.push(LineKind::Notice, format!("⏸ blocked: {r}")),
            None => self.push(LineKind::Notice, "▶ unblocked"),
        }
        self.blocked = reason;
    }

    pub fn stream(&mut self, text: &str) {
        self.streaming.push_str(text);
        self.touch();
    }

    /// Append streamed reasoning ("thinking") text, shown dimmed until the
    /// response commits.
    pub fn stream_reasoning(&mut self, text: &str) {
        self.reasoning.push_str(text);
        self.touch();
    }

    /// Commit any streamed text to the transcript as an Agent line, and drop the
    /// transient "thinking" buffer.
    pub fn commit_stream(&mut self) {
        self.touch();
        self.reasoning.clear();
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.push(LineKind::Agent, text);
        }
    }

    /// Record a turn's final answer. An *implicit* final (the model answers in
    /// plain text with no `final` tool call) streams that answer as content,
    /// which `commit_stream` has just committed as an Agent line — so re-tag that
    /// line as Final rather than appending an identical copy. An explicit `final`
    /// tool call streams no content, so the last line won't match and we append.
    pub fn push_final(&mut self, m: impl Into<String>) {
        self.touch();
        let m = m.into();
        let dup = self
            .transcript
            .last()
            .is_some_and(|l| l.kind == LineKind::Agent && l.text.trim() == m.trim());
        if dup {
            if let Some(last) = self.transcript.last_mut() {
                last.kind = LineKind::Final;
            }
        } else {
            self.push(LineKind::Final, m);
        }
    }

    pub fn tick(&mut self) {
        self.throbber.calc_next();
    }

    /// Current input text.
    pub fn input_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Feed a key event to the input editor.
    pub fn input_event(&mut self, event: crossterm::event::Event) {
        self.textarea.input(event);
    }

    /// Insert a newline at the cursor (multi-line input).
    pub fn input_newline(&mut self) {
        self.textarea.insert_newline();
    }

    /// Insert pasted text at the cursor (handles embedded newlines).
    pub fn input_paste(&mut self, text: &str) {
        self.textarea.insert_str(text);
    }

    /// Replace the input with `text`, cursor at the end (history recall).
    pub fn set_input(&mut self, text: &str) {
        let mut ta = TextArea::default();
        ta.insert_str(text);
        self.textarea = ta;
    }

    /// The cursor's row within the input editor (0-based).
    pub fn input_cursor_row(&self) -> usize {
        self.textarea.cursor().0
    }

    /// Number of lines in the input editor.
    pub fn input_lines(&self) -> usize {
        self.textarea.lines().len()
    }

    /// Clear the input editor, returning its prior content.
    pub fn take_input(&mut self) -> String {
        let text = self.input_text();
        self.textarea = TextArea::default();
        text
    }

    /// True when the input editor has no (non-whitespace) text.
    pub fn input_is_empty(&self) -> bool {
        self.input_text().trim().is_empty()
    }

    // --- multiple-choice question (ask_user pick-list) ---

    /// Enter choice mode for a question + its options.
    pub fn begin_choice(&mut self, question: String, options: Vec<String>) {
        self.textarea = TextArea::default();
        self.choice = Some(Choice {
            question,
            options,
            selected: 0,
        });
        self.mode = Mode::AwaitingChoice;
    }

    /// Move the highlighted option by `delta` (wrapping).
    pub fn choice_move(&mut self, delta: isize) {
        if let Some(c) = &mut self.choice {
            let n = c.options.len();
            if n == 0 {
                return;
            }
            c.selected = ((c.selected as isize + delta).rem_euclid(n as isize)) as usize;
        }
    }

    /// The option at `idx`, if any (for digit shortcuts).
    pub fn choice_option(&self, idx: usize) -> Option<String> {
        self.choice
            .as_ref()
            .and_then(|c| c.options.get(idx).cloned())
    }

    /// The answer to submit: a typed custom answer if present, else the
    /// highlighted option. Clears choice + input state.
    pub fn choice_answer(&mut self) -> String {
        let typed = self.take_input();
        let answer = if !typed.trim().is_empty() {
            typed.trim().to_string()
        } else {
            self.choice
                .as_ref()
                .and_then(|c| c.options.get(c.selected).cloned())
                .unwrap_or_default()
        };
        self.choice = None;
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::render::{compute_link_hits, fmt_cost, fmt_count, transcript_link_urls};
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &App) -> String {
        let mut term = Terminal::new(TestBackend::new(72, 18)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out = out.trim_end().to_string();
            out.push('\n');
        }
        out
    }

    #[test]
    fn input_box_grows_with_content_up_to_five_lines() {
        let mut app = App::new("cowboy");
        assert_eq!(app.input_lines(), 1);
        // Six lines of input; the box should cap its visible height at 5 (+borders).
        app.textarea = ratatui_textarea::TextArea::from(
            (1..=6).map(|i| format!("line {i}")).collect::<Vec<_>>(),
        );
        assert_eq!(app.input_lines(), 6);
        let frame = render(&app);
        // The box grew to its 5-line cap: lines 1..=5 are all visible at once.
        for i in 1..=5 {
            assert!(
                frame.contains(&format!("line {i}")),
                "input should show {i} lines:\n{frame}"
            );
        }
    }

    #[test]
    fn choice_selection_and_custom_answer() {
        let mut app = App::new("cowboy");
        app.begin_choice(
            "Which DB?".into(),
            vec!["postgres".into(), "sqlite".into(), "mysql".into()],
        );
        assert_eq!(app.mode, Mode::AwaitingChoice);
        // Move selection: down twice, up once -> index 1.
        app.choice_move(1);
        app.choice_move(1);
        app.choice_move(-1);
        assert_eq!(app.choice.as_ref().unwrap().selected, 1);
        // With no typed text, the answer is the highlighted option.
        assert_eq!(app.choice_answer(), "sqlite");
        assert!(app.choice.is_none());

        // A typed custom answer wins over the selection.
        app.begin_choice("Which DB?".into(), vec!["postgres".into()]);
        app.textarea.insert_str("duckdb");
        assert_eq!(app.choice_answer(), "duckdb");

        // Digit shortcut maps 1-based to the option.
        app.begin_choice("x".into(), vec!["a".into(), "b".into()]);
        assert_eq!(app.choice_option(1).as_deref(), Some("b"));
    }

    #[test]
    fn snapshot_choice_modal() {
        let mut app = App::new("cowboy");
        app.push(LineKind::Agent, "I need a decision.");
        app.begin_choice(
            "Which database should we use?".into(),
            vec!["PostgreSQL".into(), "SQLite".into(), "MySQL".into()],
        );
        app.choice_move(1); // highlight SQLite
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn following_keeps_the_newest_line_visible_even_when_lines_wrap() {
        // A transcript taller than the viewport, with long lines that WRAP — the
        // case where a char-width estimate undercounts and the tail gets hidden
        // under the input. With follow on, the last line must be on screen.
        let mut app = App::new("cowboy");
        app.mode = Mode::Idle;
        // Three ~28-char words per line: two don't fit on one ~47-wide row, so
        // word-wrap yields 3 rows while a char-width estimate guesses 2 — the
        // undercount that, accumulated over many lines, hides the tail.
        let w = "a".repeat(28);
        let long = format!("{w} {w} {w}");
        for i in 0..40 {
            app.push(LineKind::Agent, format!("{i} {long}"));
        }
        app.push(LineKind::Final, "LAST_LINE_SENTINEL");
        assert!(app.follow, "new content should keep us following the tail");

        let frame = render(&app);
        assert!(
            frame.contains("LAST_LINE_SENTINEL"),
            "the newest line must be visible when following:\n{frame}"
        );
    }

    #[test]
    fn snapshot_running_with_panes() {
        let mut app = App::new("cowboy");
        app.status = "exec: cargo test".into();
        app.push(LineKind::User, "fix the failing test");
        app.push(LineKind::Command, "cargo test");
        app.push(LineKind::Output, "test result: FAILED");
        app.activity("ask example.com:443");
        app.stream("Looking at the failure...");
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn snapshot_background_jobs_and_queue() {
        // The three states that matter while work is delegated: one running, one queued
        // behind the provider cap, and one parked waiting for the foreman to answer a
        // turn request — plus deferred user input, all visible at a glance.
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "review the storage crates");
        app.apply_jobs(vec![
            CrewMember {
                id: "s1".into(),
                label: "review/redb".into(),
                model: "anthropic/opus".into(),
                status: CrewStatus::Running,
                started_ms: 0,
                elapsed_secs: 42,
                used: 30,
                granted: 60,
                ceiling: 400,
                requested: 0,
            },
            CrewMember {
                id: "s2".into(),
                label: "review/columnar".into(),
                model: "anthropic/opus".into(),
                status: CrewStatus::Pending,
                started_ms: 0,
                elapsed_secs: 0,
                used: 0,
                granted: 60,
                ceiling: 400,
                requested: 0,
            },
            CrewMember {
                id: "s3".into(),
                label: "tests/small".into(),
                model: "cheap".into(),
                status: CrewStatus::Asking,
                started_ms: 0,
                elapsed_secs: 120,
                used: 25,
                granted: 25,
                ceiling: 400,
                requested: 30,
            },
        ]);
        app.queued = vec!["and then update the docs".into()];
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn a_job_snapshot_replaces_the_pane_and_counts_only_live_work() {
        let mut app = App::new("cowboy");
        let member = |id: &str, status| CrewMember {
            id: id.into(),
            label: "l".into(),
            model: "m".into(),
            status,
            started_ms: 0,
            elapsed_secs: 1,
            used: 0,
            granted: 0,
            ceiling: 0,
            requested: 0,
        };
        app.apply_jobs(vec![
            member("a", CrewStatus::Running),
            member("b", CrewStatus::Asking),
            member("c", CrewStatus::Done),
            member("d", CrewStatus::Failed),
        ]);
        // A worker waiting for a decision is still live work — it is *waiting on us*,
        // which is precisely when it must not be hidden.
        assert_eq!(app.live_jobs(), 2);
        let s = app.jobs_summary();
        assert!(s.contains("waiting for your decision"), "got: {s}");
        // The snapshot is authoritative: a later one replaces the pane rather than
        // accumulating stale rows.
        app.apply_jobs(vec![member("a", CrewStatus::Done)]);
        assert_eq!(app.crew.len(), 1);
        assert_eq!(app.live_jobs(), 0);
    }

    #[test]
    fn snapshot_markdown_and_diff_rendering() {
        let mut app = App::new("cowboy");
        app.mode = Mode::Idle;
        app.status = "ready".into();
        app.push(LineKind::User, "summarize the change");
        app.push(
            LineKind::Final,
            "## Summary\n\nFixed the **guard** in `placeholders_util.rb`.\n\n| Case | Before | After |\n|---|---|---|\n| nil | leak | empty |\n\n```rust\nfn ok() -> bool { true }\n```",
        );
        app.push(
            LineKind::Diff,
            "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,3 +1,3 @@\n fn ok() {\n-    old();\n+    new();\n }",
        );
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn snapshot_welcome_screen() {
        let mut app = App::new("cowboy · 20260614-abcd");
        for l in [
            "Welcome to cowboy — the agent runs in a sandbox built from your machine.",
            "workspace  /home/dev/myproject",
            "model      anthropic/claude-sonnet-4-6  (gw.local)",
            "branch     main",
            "",
            "Type a message to begin · Enter sends · Ctrl-C menu",
        ] {
            app.push(LineKind::Banner, l);
        }
        app.mode = Mode::Idle;
        app.status = "ready".into();
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn scrolling_detaches_from_and_returns_to_tail() {
        let mut app = App::new("cowboy");
        for i in 0..100 {
            app.push(LineKind::Output, format!("line {i}"));
        }
        // Force max_scroll to be computed.
        let _ = render(&app);
        assert!(app.at_bottom());
        app.scroll_up(5);
        assert!(!app.at_bottom());
        app.scroll_to_bottom();
        assert!(app.at_bottom());
    }

    #[test]
    fn snapshot_approval_modal() {
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "build the project");
        app.mode = Mode::Approval("github.com:443".into());
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn snapshot_approval_modal_names_the_command_that_asked() {
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "build the project");
        app.mode = Mode::Approval(
            "crates.io:443\n\nrequested by:  cargo test --workspace --all-targets".into(),
        );
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn an_approval_legend_is_not_clipped_by_a_long_command() {
        // The modal sizes itself from the body, and used to count source lines rather than
        // wrapped rows — so naming the command pushed the once/session/project/global
        // legend off the bottom, leaving a prompt with no visible way to answer it.
        let long = "requested by:  bash -c 'curl -sSL https://example.com/very/long/path \
                    | tar xz -C /tmp/somewhere && ./configure --with-everything'";
        let mut app = App::new("cowboy");
        app.mode = Mode::Approval(format!("example.com:443\n\n{long}"));
        let out = render(&app);
        for key in [
            "o  once",
            "s  session",
            "p  project",
            "g  global",
            "d  deny",
        ] {
            assert!(out.contains(key), "{key:?} was clipped:\n{out}");
        }
        assert!(out.contains("Esc = deny"), "the footer was clipped:\n{out}");
    }

    #[test]
    fn snapshot_help_overlay() {
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "do work");
        app.open_help(
            vec![
                HelpSection {
                    title: "Steering a run".into(),
                    rows: vec![
                        ("Ctrl-C".into(), "interrupt the turn".into()),
                        ("/after <msg>".into(), "queue for after this turn".into()),
                    ],
                },
                HelpSection {
                    title: "Delegated work".into(),
                    rows: vec![("Alt-j".into(), "what the subagents are doing".into())],
                },
            ],
            None,
        );
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn a_long_help_body_scrolls_instead_of_being_truncated() {
        // The regression this replaces: `/help` printed 25 lines into the transcript, and
        // a fixed-size modal would simply have clipped them. Here the tail must be
        // reachable — and scrolling must stop at the end rather than run off into blanks.
        let rows: Vec<(String, String)> = (0..40)
            .map(|i| (format!("key{i}"), format!("does thing {i}")))
            .collect();
        let mut app = App::new("cowboy");
        app.open_help(
            vec![HelpSection {
                title: "Lots".into(),
                rows,
            }],
            None,
        );
        // A render measures the viewport, which is what paging is relative to.
        let first = render(&app);
        assert!(first.contains("key0"));
        assert!(first.contains("more below"));

        let h = app.help.as_mut().unwrap();
        let (viewport, total) = (h.viewport.get(), h.total.get());
        assert!(viewport > 0, "the render should have reported a viewport");
        assert!(total > viewport, "40 rows should not fit in one page");
        h.scroll_by(1000);
        let at_end = h.scroll;
        assert_eq!(at_end, total - viewport, "clamped to a full last page");
        let last = render(&app);
        assert!(last.contains("key39"), "the tail is reachable:\n{last}");

        // And scrolling further changes nothing.
        app.help.as_mut().unwrap().scroll_by(1000);
        assert_eq!(app.help.as_ref().unwrap().scroll, at_end);
        app.help.as_mut().unwrap().scroll_by(-1000);
        assert_eq!(app.help.as_ref().unwrap().scroll, 0);
    }

    #[test]
    fn snapshot_completion_popup() {
        let mut app = App::new("cowboy");
        app.mode = Mode::Idle;
        app.set_input("/gi");
        app.set_completions(vec![
            Completion {
                value: "github:review-pr".into(),
                hint: "<pr-number-or-url> [filename]".into(),
            },
            Completion {
                value: "git:setup-worktree".into(),
                hint: "set up a new worktree".into(),
            },
        ]);
        app.completion_move(1); // select the second
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn completion_accept_replaces_input() {
        let mut app = App::new("cowboy");
        app.set_input("/git");
        app.set_completions(vec![Completion {
            value: "github:review-pr".into(),
            hint: "h".into(),
        }]);
        app.accept_completion();
        assert_eq!(app.input_text(), "/github:review-pr ");
        assert!(!app.has_completions());
    }

    fn sample_choice(id: &str, label: &str, configured: bool, current: bool) -> ModelChoice {
        ModelChoice {
            id: id.into(),
            label: label.into(),
            configured,
            current,
            configured_name: configured.then(|| label.to_string()),
            suggested_name: label.into(),
            context_window: 131072,
            max_tokens: 16384,
            temperature: 0.6,
            reasoning: Some("high".into()),
        }
    }

    #[test]
    fn snapshot_model_picker() {
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "switch models");
        app.model_picker = Some(ModelPicker {
            entries: vec![
                sample_choice("cerebras/zai-glm-4.7", "Cerebras: GLM 4.7", true, true),
                sample_choice(
                    "fireworks/accounts/fireworks/models/glm-5p1",
                    "Fireworks: GLM 5.1",
                    false,
                    false,
                ),
                sample_choice("anthropic/claude-opus-4-8", "Claude Opus 4.8", false, false),
            ],
            filter: String::new(),
            selected: 1,
            crew_mode: true,
        });
        app.mode = Mode::ModelPicker;
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn snapshot_model_form() {
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "configure a model");
        app.model_form = Some(ModelForm::from_choice(&sample_choice(
            "fireworks/accounts/fireworks/models/glm-5p1",
            "Fireworks: GLM 5.1",
            false,
            false,
        )));
        app.mode = Mode::ModelForm;
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn snapshot_token_total_in_status_bar() {
        let mut app = App::new("~/dev/cowboy  ⎇ main");
        app.push(LineKind::User, "refactor the parser");
        app.push(LineKind::Final, "Done.");
        app.mode = Mode::Idle;
        app.status = "ready".into();
        app.tokens_in = 128_300;
        app.tokens_out = 9_120;
        app.diff = "Δ 2f +30 -4".into();
        insta::assert_snapshot!(render(&app));
    }

    /// The `/context` report is the answer to "why is my window full?", so what it
    /// prints is worth pinning: the ratio, the reserve, and the consumers ranked.
    #[test]
    fn snapshot_context_report() {
        let mut app = App::new("~/dev/cowboy  ⎇ main");
        app.mode = Mode::Idle;
        app.status = "ready".into();
        app.context = Some(ContextSnapshot {
            used: 84_500,
            budget: 160_000,
            window: 200_000,
            reserve: 40_000,
            top: vec![
                ("tool results".into(), 41_000),
                ("model reasoning".into(), 22_000),
                ("assistant messages".into(), 12_500),
                ("tool schemas".into(), 3_650),
                ("your messages".into(), 2_400),
                ("system + summaries".into(), 1_300),
            ],
        });
        // Rendered by the CLI's `/context` handler; mirrored here so the layout is
        // reviewable. Percentages and bars are computed from the snapshot.
        let c = app.context.clone().unwrap();
        assert_eq!(c.percent(), 52);
        app.push(LineKind::Notice, "context  84,500/160,000 tokens (52%)");
        for (label, n) in &c.top {
            let share = (*n * 20 / c.used).min(20);
            app.push(
                LineKind::Notice,
                format!("  {:<20} {:>9}  {}", label, n, "█".repeat(share as usize)),
            );
        }
        insta::assert_snapshot!(render(&app));
    }

    /// A budget of zero means the window cannot even hold the reserve. Reporting it as
    /// 100% is honest; dividing by it is not.
    #[test]
    fn context_percent_handles_a_zero_budget() {
        let c = ContextSnapshot {
            used: 10,
            budget: 0,
            ..Default::default()
        };
        assert_eq!(c.percent(), 100);
        let empty = ContextSnapshot {
            used: 0,
            budget: 1000,
            ..Default::default()
        };
        assert_eq!(empty.percent(), 0);
    }

    #[test]
    fn selection_copies_transcript_text_excluding_sidebar() {
        let mut app = App::new("t");
        app.push(LineKind::Agent, "hello world from the transcript");
        app.activity("ask example.com:443"); // lives in the right-hand pane
        let mut term = Terminal::new(TestBackend::new(72, 18)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();

        // Select the full first wrapped line of the transcript.
        let w = app.transcript_area.get().width;
        app.selection = Some(Selection {
            anchor: (0, 0),
            cursor: (0, w - 1),
        });
        let text = app.selected_text().unwrap();
        assert!(
            text.contains("hello world from the transcript"),
            "got {text:?}"
        );
        // Extraction renders only the transcript paragraph, so the sidebar
        // (a separate pane) can never bleed in.
        assert!(!text.contains("example.com"), "sidebar leaked: {text:?}");

        // A bare click (no drag) copies nothing.
        app.selection = Some(Selection {
            anchor: (0, 0),
            cursor: (0, 0),
        });
        assert!(app.selected_text().is_none());
    }

    #[test]
    fn selection_spans_scrollback_beyond_the_viewport() {
        // A transcript taller than the viewport. A selection anchored to logical
        // wrapped-line indices must extract text that isn't currently on screen.
        let mut app = App::new("t");
        // Output lines render 1:1 (no blank spacers between them), so logical
        // wrapped-line index maps directly to content line index here.
        for i in 0..100 {
            app.push(LineKind::Output, format!("transcript line {i}"));
        }
        let mut term = Terminal::new(TestBackend::new(72, 18)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let w = app.transcript_area.get().width;
        // Lines 2..=5 are near the top — scrolled out of view when following.
        app.selection = Some(Selection {
            anchor: (2, 0),
            cursor: (5, w - 1),
        });
        let text = app.selected_text().expect("offscreen selection extracts");
        assert!(text.contains("transcript line 2"), "got {text:?}");
        assert!(text.contains("transcript line 5"), "got {text:?}");
        assert_eq!(text.lines().count(), 4, "four wrapped lines: {text:?}");
    }

    #[test]
    fn transient_command_output_overwrites_in_place() {
        let mut app = App::new("t");
        app.start_command("build", 1000);
        app.command_output_line("compiling", true); // committed line
        app.command_output_line("[ 10%]", false); // transient progress
        app.command_output_line("[ 50%]", false); // overwrites the progress line
        app.command_output_line("[100%]", true); // overwrites, then commits
        app.command_output_line("done", true); // new committed line
        let out: Vec<&str> = app
            .transcript
            .iter()
            .filter(|l| l.kind == LineKind::Output)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(out, vec!["compiling", "[100%]", "done"]);
        assert_eq!(app.running.as_ref().unwrap().last, "done");
        app.tick_command(4000);
        assert_eq!(app.running.as_ref().unwrap().elapsed_secs, 3);
        app.end_command();
        assert!(app.running.is_none());
    }

    #[test]
    fn verbose_command_output_collapses_to_a_bounded_tail() {
        let mut app = App::new("t");
        app.start_command("mise install", 1000);
        for i in 0..40 {
            app.command_output_line(format!("line {i}"), true);
        }
        let lines: Vec<&str> = app
            .transcript
            .iter()
            .map(|l| l.text.as_str())
            .filter(|t| t.starts_with("line ") || t.starts_with("⋯"))
            .collect();
        // At most one marker + the 12-line tail (the most recent lines).
        let outputs: Vec<&&str> = lines.iter().filter(|t| t.starts_with("line ")).collect();
        assert_eq!(outputs.len(), 12, "tail bounded to 12 output lines");
        assert_eq!(*outputs.first().unwrap(), &"line 28");
        assert_eq!(*outputs.last().unwrap(), &"line 39");
        // The marker sits immediately above the tail and counts the hidden lines.
        let marker = lines.iter().find(|t| t.starts_with("⋯")).unwrap();
        assert_eq!(*marker, "⋯ 28 earlier lines hidden");
        assert_eq!(
            lines[0], "⋯ 28 earlier lines hidden",
            "marker is above the tail"
        );
        // The live indicator still reflects the newest line.
        assert_eq!(app.running.as_ref().unwrap().last, "line 39");
    }

    #[test]
    fn implicit_final_is_not_rendered_twice() {
        // Implicit final: the model streams its answer as content (committed as an
        // Agent line by commit_stream), then signals Final with the same text.
        let mut app = App::new("t");
        app.stream("the full PR review report");
        app.commit_stream();
        app.push_final("the full PR review report");
        let lines: Vec<(&LineKind, &str)> = app
            .transcript
            .iter()
            .map(|l| (&l.kind, l.text.as_str()))
            .collect();
        assert_eq!(
            lines,
            vec![(&LineKind::Final, "the full PR review report")],
            "the answer is shown once, tagged Final"
        );

        // Explicit final tool call: no streamed content, so the final is appended.
        let mut app = App::new("t");
        app.commit_stream(); // nothing streamed
        app.push_final("done; tests pass");
        assert_eq!(app.transcript.len(), 1);
        assert_eq!(app.transcript[0].kind, LineKind::Final);

        // Explicit final after a distinct preamble keeps both lines.
        let mut app = App::new("t");
        app.stream("Let me summarize:");
        app.commit_stream();
        app.push_final("## Summary\n...");
        assert_eq!(app.transcript.len(), 2);
        assert_eq!(app.transcript[0].kind, LineKind::Agent);
        assert_eq!(app.transcript[1].kind, LineKind::Final);
    }

    #[test]
    fn streaming_buffer_renders_markdown_live() {
        // The in-progress (uncommitted) answer formats as it streams, not only
        // once committed — so a heading/bold/list resolves live.
        let mut app = App::new("t");
        app.stream("## Plan\n\n- step **one**");
        let lines = transcript_lines(&app);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(texts.iter().any(|t| t.contains("Plan")));
        assert!(texts.iter().any(|t| t.contains("• step")));
        assert!(texts.iter().any(|t| t.contains("one")));
    }

    #[test]
    fn osc8_hyperlink_is_overlaid_on_link_cells() {
        // A rendered link's cells get rewritten to carry the OSC 8 escape with
        // the hidden URL, so the label is clickable without showing the URL.
        let mut app = App::new("t");
        app.push(LineKind::Agent, "see [the spec](https://example.com/x)");
        let width = 60u16;
        let lines = transcript_lines(&app);
        let hits = compute_link_hits(&lines, width, &transcript_link_urls(&app));
        assert_eq!(hits.len(), 1, "one link found");
        assert_eq!(hits[0].url, "https://example.com/x");

        // Render + overlay into a buffer; a cell on the link row carries the escape.
        let mut term = Terminal::new(TestBackend::new(width + 2, 10)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let found = (0..buf.area.width)
            .flat_map(|x| (0..buf.area.height).map(move |y| (x, y)))
            .any(|(x, y)| {
                buf[(x, y)]
                    .symbol()
                    .contains("\x1b]8;;https://example.com/x\x07")
            });
        assert!(
            found,
            "an OSC 8 escape with the URL is present in the buffer"
        );
    }

    #[test]
    fn line_cache_reuses_until_content_changes() {
        let mut app = App::new("t");
        app.push(LineKind::Agent, "hi");
        let v1 = app.content_ver.get();
        let _ = transcript_lines(&app);
        let _ = transcript_lines(&app); // repeated renders don't bump the version
        assert_eq!(app.content_ver.get(), v1, "drawing must not invalidate");
        app.stream("more");
        assert_ne!(app.content_ver.get(), v1, "a content change invalidates");
    }

    #[test]
    fn crew_pane_tracks_subagents() {
        let mut app = App::new("t");
        app.subagent_started(
            "review",
            "fireworks/accounts/fireworks/models/minimax-m3",
            "r1",
            1000,
        );
        app.subagent_started("tests", "cerebras/zai-glm-4.7", "t1", 1000);
        assert_eq!(app.crew.len(), 2);
        app.tick_crew(5000);
        assert_eq!(app.crew[0].elapsed_secs, 4);

        // Finishing (matched by id) freezes elapsed; status reflects success.
        app.subagent_done("t1", true);
        app.tick_crew(9000);
        let tests = app.crew.iter().find(|m| m.id == "t1").unwrap();
        assert_eq!(tests.status, CrewStatus::Done);
        assert_eq!(tests.elapsed_secs, 4, "done member's time is frozen");
        assert_eq!(app.crew[0].elapsed_secs, 8, "running member keeps ticking");

        app.subagent_done("r1", false);
        assert_eq!(app.crew[0].status, CrewStatus::Failed);

        // A fresh fan-out (no member still running) replaces the previous batch.
        app.subagent_started("docs", "glm", "d1", 10000);
        assert_eq!(app.crew.len(), 1);
        assert_eq!(app.crew[0].label, "docs");
    }

    #[test]
    fn freeze_crew_stops_running_timers() {
        let mut app = App::new("t");
        app.subagent_started("review", "m", "r1", 1000);
        app.tick_crew(5000);
        assert_eq!(app.crew[0].elapsed_secs, 4);
        // Session ended while the subagent was still running: freeze it.
        app.freeze_crew();
        app.tick_crew(60_000);
        assert_eq!(app.crew[0].status, CrewStatus::Failed);
        assert_eq!(app.crew[0].elapsed_secs, 4, "frozen, not still ticking");
    }

    #[test]
    fn pending_subagents_are_distinct_from_running() {
        let mut app = App::new("t");
        // A batch is announced as pending (queued behind the per-provider cap).
        app.subagent_pending("core", "glm", "c1", 1000);
        app.subagent_pending("gw", "glm", "g1", 1000);
        assert_eq!(app.crew.len(), 2);
        assert!(app.crew.iter().all(|m| m.status == CrewStatus::Pending));

        // Pending members don't tick — the timer only runs once they start.
        app.tick_crew(5000);
        assert_eq!(app.crew[0].elapsed_secs, 0, "pending must not tick");

        // One acquires a permit and starts; it flips in place (no new entry),
        // and its clock starts from the start time, not the queue time.
        app.subagent_started("core", "glm", "c1", 6000);
        assert_eq!(
            app.crew.len(),
            2,
            "started flips the pending entry, not appends"
        );
        assert_eq!(app.crew[0].status, CrewStatus::Running);
        assert_eq!(app.crew[1].status, CrewStatus::Pending);
        app.tick_crew(9000);
        assert_eq!(
            app.crew[0].elapsed_secs, 3,
            "running clock from start, not queue"
        );
        assert_eq!(
            app.crew[1].elapsed_secs, 0,
            "still-pending member stays at 0"
        );

        // Freeze marks both running and pending as failed (session ended).
        app.freeze_crew();
        assert_eq!(app.crew[0].status, CrewStatus::Failed);
        assert_eq!(app.crew[1].status, CrewStatus::Failed);
    }

    #[test]
    fn transcript_buffer_is_bounded() {
        let mut app = App::new("t");
        for i in 0..8000 {
            app.push(LineKind::Output, format!("line {i}"));
        }
        // Bounded (cap + slack), oldest dropped, newest kept.
        assert!(
            app.transcript.len() <= 5000 + 512,
            "len {}",
            app.transcript.len()
        );
        assert!(app.transcript.len() >= 5000);
        assert_eq!(app.transcript.last().unwrap().text, "line 7999");
        assert!(!app.transcript.iter().any(|l| l.text == "line 0"));
    }

    #[test]
    fn network_activity_aggregates_by_verdict_and_host() {
        let mut app = App::new("t");
        app.activity("allow github.com:443 (allowed by policy (domain github.com))");
        app.activity("allow github.com:443 (allowed by policy (domain github.com))");
        app.activity("deny evil.com:443 (blocked)");
        app.activity("allow github.com:443 (allowed by policy)"); // same key, diff reason
                                                                  // Two distinct rows; the repeated github.com collapsed to a count of 3.
        assert_eq!(app.activity.len(), 2);
        let gh = app
            .activity
            .iter()
            .find(|e| e.verdict == "allow" && e.host == "github.com:443")
            .unwrap();
        assert_eq!(gh.count, 3);
        assert_eq!(app.activity[1].verdict, "deny");
        assert_eq!(app.activity[1].count, 1);
        // A line with no reason still parses to verdict + host.
        app.activity("ask api.github.com:443");
        let ask = app.activity.last().unwrap();
        assert_eq!(
            (ask.verdict.as_str(), ask.host.as_str()),
            ("ask", "api.github.com:443")
        );
    }

    #[test]
    fn begin_selection_only_inside_transcript() {
        let mut app = App::new("t");
        app.transcript_area.set(Rect::new(1, 1, 40, 10));
        app.begin_selection(5, 5);
        assert!(app.has_selection());
        app.begin_selection(60, 5); // outside (sidebar) -> clears
        assert!(!app.has_selection());
    }

    #[test]
    fn fmt_count_is_compact() {
        assert_eq!(fmt_count(980), "980");
        assert_eq!(fmt_count(1200), "1.2k");
        assert_eq!(fmt_count(45_000), "45k");
        assert_eq!(fmt_count(1_250_000), "1.2M");
    }

    #[test]
    fn fmt_cost_handles_tiny_and_normal_spend() {
        assert_eq!(fmt_cost(0.0), "0.00");
        assert_eq!(fmt_cost(0.002), "<0.01");
        assert_eq!(fmt_cost(0.04), "0.04");
        assert_eq!(fmt_cost(12.3), "12.30");
    }

    #[test]
    fn snapshot_indicators_diff_and_processes() {
        let mut app = App::new("cowboy · 20260614-abcd");
        app.push(LineKind::User, "start the dev server");
        app.push(LineKind::Final, "Server running on :3000.");
        app.mode = Mode::Idle;
        app.status = "ready".into();
        app.diff = "Δ 2f +30 -4".into();
        app.processes = vec![("web".into(), "running".into())];
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn snapshot_plan_pane() {
        let mut app = App::new("cowboy · 20260614-abcd");
        app.push(LineKind::User, "refactor the parser");
        app.mode = Mode::Running;
        app.status = "working".into();
        app.plan = vec![
            ("read the existing parser".into(), "done".into()),
            ("extract the tokenizer".into(), "in_progress".into()),
            ("add tests".into(), "pending".into()),
        ];
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn plan_pane_only_shown_when_plan_is_nonempty() {
        let mut app = App::new("cowboy");
        // No plan: the rendered frame must not carry the plan panel title.
        assert!(!render(&app).contains("plan "));
        app.plan = vec![("do the thing".into(), "pending".into())];
        assert!(render(&app).contains("plan 0/1"));
    }

    #[test]
    fn snapshot_idle_ready_for_next_message() {
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "create a hello world");
        app.push(LineKind::Final, "Done — created main.rs.");
        app.mode = Mode::Idle;
        app.status = "ready".into();
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn commit_stream_moves_streaming_into_transcript() {
        let mut app = App::new("t");
        app.stream("hello");
        app.commit_stream();
        assert_eq!(app.transcript.len(), 1);
        assert_eq!(app.transcript[0].kind, LineKind::Agent);
        assert!(app.streaming.is_empty());
    }

    #[test]
    fn input_editing_via_textarea() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("t");
        for c in ['h', 'i'] {
            app.input_event(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(app.input_text(), "hi");
        assert_eq!(app.take_input(), "hi");
        assert_eq!(app.input_text(), "");
    }
}
