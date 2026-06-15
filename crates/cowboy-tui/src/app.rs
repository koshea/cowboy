//! Renderable TUI state and drawing. The CLI owns the event loop and feeds
//! this `App`; here we keep state + a pure `draw` so rendering is
//! snapshot-testable with `ratatui::backend::TestBackend`.

use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use ratatui_textarea::TextArea;
use throbber_widgets_tui::{Throbber, ThrobberState};

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
    Paused,
    /// Choosing a model from the provider catalogue (state in `App::model_picker`).
    ModelPicker,
    /// Configuring a newly chosen model (state in `App::model_form`).
    ModelForm,
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

pub struct App {
    pub title: String,
    pub status: String,
    /// Working-tree diff summary for the status bar (e.g. `Δ 2 files +30 -4`).
    pub diff: String,
    /// Running session token estimate (input/prompt, output/completion).
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Running estimated session spend in USD (0.0 when pricing is unknown).
    pub cost_usd: f64,
    pub transcript: Vec<TranscriptLine>,
    /// In-progress streamed agent text (not yet committed to the transcript).
    pub streaming: String,
    /// In-progress streamed "thinking" (reasoning), shown dimmed and cleared
    /// when the response commits. Never added to the transcript.
    pub reasoning: String,
    /// Network activity log (gateway decisions).
    pub activity: Vec<String>,
    /// Managed processes: (name, status).
    pub processes: Vec<(String, String)>,
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
    /// Input editor (multi-line, cursor) via ratatui-textarea.
    pub textarea: TextArea<'static>,
    pub mode: Mode,
    pub throbber: ThrobberState,
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
    /// Model-catalogue picker state (set while `mode == ModelPicker`).
    pub model_picker: Option<ModelPicker>,
    /// New-model config form state (set while `mode == ModelForm`).
    pub model_form: Option<ModelForm>,
    /// Pending multiple-choice question (set while `mode == AwaitingChoice`).
    pub choice: Option<Choice>,
    /// Slash-command autocomplete popup (set while the input is `/<partial>`).
    pub completion: Option<CompletionState>,
    /// Text awaiting copy to the system clipboard. The event loop drains it
    /// *after* the frame is flushed, writing the OSC 52 sequence through
    /// ratatui's own backend — writing to an independent stdout handle here
    /// races crossterm's buffered frame and the bytes get eaten.
    pub pending_copy: Option<String>,
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
            cost_usd: 0.0,
            transcript: Vec::new(),
            streaming: String::new(),
            reasoning: String::new(),
            activity: Vec::new(),
            processes: Vec::new(),
            plan: Vec::new(),
            blocked: None,
            running: None,
            last_output_transient: false,
            textarea: TextArea::default(),
            mode: Mode::Running,
            throbber: ThrobberState::default(),
            follow: true,
            scroll_top: 0,
            max_scroll: std::cell::Cell::new(0),
            selection: None,
            selecting: false,
            scroll_offset: std::cell::Cell::new(0),
            followed_before_select: false,
            transcript_area: std::cell::Cell::new(Rect::ZERO),
            model_picker: None,
            model_form: None,
            choice: None,
            completion: None,
            pending_copy: None,
        }
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
        let para = Paragraph::new(build_transcript_lines(self))
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

    pub fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        // Any non-output line ends a transient progress run (the next output
        // chunk must append, not overwrite this line).
        if kind != LineKind::Output {
            self.last_output_transient = false;
        }
        self.transcript.push(TranscriptLine {
            kind,
            text: text.into(),
        });
    }

    /// Mark the start of a streamed shell command (for the live indicator).
    pub fn start_command(&mut self, cmd: impl Into<String>, now_ms: u64) {
        self.last_output_transient = false;
        self.running = Some(RunningCmd {
            cmd: cmd.into(),
            started_ms: now_ms,
            elapsed_secs: 0,
            last: String::new(),
        });
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

    /// Append (or, for a transient carriage-return update, overwrite-in-place) a
    /// line of streamed command output, and update the live tail.
    pub fn command_output_line(&mut self, text: impl Into<String>, committed: bool) {
        let text = text.into();
        let replace = self.last_output_transient
            && self
                .transcript
                .last()
                .is_some_and(|l| l.kind == LineKind::Output);
        if replace {
            if let Some(last) = self.transcript.last_mut() {
                last.text = text.clone();
            }
        } else {
            self.push(LineKind::Output, text.clone());
        }
        self.last_output_transient = !committed;
        if let Some(r) = &mut self.running {
            r.last = text;
        }
    }

    /// Append a network activity line.
    pub fn activity(&mut self, line: impl Into<String>) {
        self.activity.push(line.into());
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
    }

    /// Append streamed reasoning ("thinking") text, shown dimmed until the
    /// response commits.
    pub fn stream_reasoning(&mut self, text: &str) {
        self.reasoning.push_str(text);
    }

    /// Commit any streamed text to the transcript as an Agent line, and drop the
    /// transient "thinking" buffer.
    pub fn commit_stream(&mut self) {
        self.reasoning.clear();
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.push(LineKind::Agent, text);
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

fn style_for(kind: LineKind) -> (&'static str, Style) {
    match kind {
        LineKind::Banner => ("", Style::default().fg(Color::DarkGray)),
        LineKind::User => (
            "› ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        LineKind::Agent => ("", Style::default().fg(Color::Cyan)),
        LineKind::Command => (
            "$ ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        LineKind::Tool => ("✎ ", Style::default().fg(Color::Magenta)),
        LineKind::Output => ("  ", Style::default().fg(Color::Gray)),
        LineKind::Final => (
            "✓ ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        LineKind::Notice => ("", Style::default().fg(Color::DarkGray)),
        LineKind::Error => ("✗ ", Style::default().fg(Color::Red)),
    }
}

/// Insert a blank spacer before this entry when it starts a new "block" so the
/// transcript breathes (e.g. before a user turn or the final summary).
fn spacer_before(prev: Option<LineKind>, cur: LineKind) -> bool {
    let Some(prev) = prev else { return false };
    if prev == cur && matches!(cur, LineKind::Output | LineKind::Command | LineKind::Tool) {
        return false;
    }
    matches!(
        cur,
        LineKind::User | LineKind::Final | LineKind::Command | LineKind::Tool | LineKind::Agent
    ) && prev != LineKind::Banner
}

/// Draw the whole UI.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    // The input box grows with its content, up to 5 visible lines (+2 borders).
    let input_h = (app.input_lines().clamp(1, 5) as u16) + 2;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),          // main
            Constraint::Length(1),       // status bar
            Constraint::Length(input_h), // input (grows to 5 lines)
        ])
        .split(area);

    // Main row: transcript on the left, side panels on the right.
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(rows[0]);
    draw_transcript(f, app, main[0]);

    if app.plan.is_empty() {
        let side = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(main[1]);
        draw_activity(f, app, side[0]);
        draw_processes(f, app, side[1]);
    } else {
        // With a plan, give it the top third and split the rest as before.
        let side = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(40),
                Constraint::Percentage(26),
            ])
            .split(main[1]);
        draw_plan(f, app, side[0]);
        draw_activity(f, app, side[1]);
        draw_processes(f, app, side[2]);
    }

    draw_status(f, app, rows[1]);
    draw_input(f, app, rows[2]);
    // Slash-command autocomplete floats just above the input.
    if matches!(app.mode, Mode::Idle | Mode::Running) {
        draw_completions(f, app, rows[2]);
    }

    match &app.mode {
        Mode::AwaitingInput(q) => draw_modal(f, area, "Question", q, "type your answer · Enter"),
        Mode::AwaitingChoice => {
            if let Some(c) = &app.choice {
                draw_choice(f, area, c, &app.input_text());
            }
        }
        Mode::Approval(p) => draw_modal(
            f,
            area,
            "Approval",
            p,
            "[o]nce [s]ession [p]roject [g]lobal [d]eny",
        ),
        Mode::Paused => draw_modal(
            f,
            area,
            "Paused",
            "Agent paused.",
            "[r]esume  [i]nstruct  [k]ill command  [d]etach  [e]nd session",
        ),
        Mode::ModelPicker => {
            if let Some(p) = &app.model_picker {
                draw_model_picker(f, area, p);
            }
        }
        Mode::ModelForm => {
            if let Some(form) = &app.model_form {
                draw_model_form(f, area, form);
            }
        }
        _ => {}
    }
}

/// Centered rect `w`×`h` (clamped to `area`), cleared for an overlay.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_model_picker(f: &mut Frame, area: Rect, p: &ModelPicker) {
    let rect = centered(area, 76, 20);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(Span::styled(
            " Select a model ",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Layout: filter line, list, footer.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let filter = if p.filter.is_empty() {
        Span::styled("type to filter…", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(format!("/{}", p.filter), Style::default().fg(Color::Yellow))
    };
    f.render_widget(Paragraph::new(Line::from(filter)), rows[0]);

    let entries = p.filtered();
    let view_h = rows[1].height as usize;
    // Keep the selection in view.
    let top = p.selected.saturating_sub(view_h.saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    for (i, e) in entries.iter().enumerate().skip(top).take(view_h) {
        let sel = i == p.selected;
        let marker = if sel { "› " } else { "  " };
        let mut tag = String::new();
        if e.current {
            tag.push_str(" ◉ current");
        } else if e.configured {
            tag.push_str(" • configured");
        }
        let base = if sel {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let line = format!("{marker}{:<46}{}", trunc(&e.id, 46), tag);
        lines.push(Line::from(Span::styled(line, base)));
    }
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines), rows[1]);

    // Footer: navigation hints + the Solo/Crew mode toggle (Tab).
    let (mode_label, mode_color) = if p.crew_mode {
        ("crew (delegates)", Color::Magenta)
    } else {
        ("solo", Color::Cyan)
    };
    let footer = Line::from(vec![
        Span::styled(
            "↑/↓ move · Enter select · Esc cancel · ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("Tab", Style::default().fg(Color::White)),
        Span::styled(" mode: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            mode_label,
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(footer), rows[2]);
}

fn draw_model_form(f: &mut Frame, area: Rect, form: &ModelForm) {
    let rect = centered(area, 70, 13);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(Span::styled(
            format!(" Configure {} ", trunc(&form.id, 40)),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();
    for (i, label) in ModelForm::FIELD_LABELS.iter().enumerate() {
        let focused = form.focus == i;
        let val = &form.fields[i];
        lines.push(field_line(label, val, focused));
    }
    // Reasoning effort (field index 4).
    lines.push(field_line(
        "Reasoning",
        &format!("◂ {} ▸", form.reasoning()),
        form.focus == 4,
    ));
    lines.push(Line::from(""));
    if let Some(err) = &form.error {
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Tab/↑↓ field · ◂▸ effort · Enter/Ctrl-S save · Esc back",
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// One labeled form field; the focused field is highlighted with a caret.
fn field_line(label: &str, value: &str, focused: bool) -> Line<'static> {
    let caret = if focused { "› " } else { "  " };
    let label_style = if focused {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let val_style = if focused {
        Style::default().add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{caret}{label:<16}"), label_style),
        Span::styled(value.to_string(), val_style),
    ])
}

/// Truncate a string to `max` chars with an ellipsis.
fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Build the transcript as a flat list of (unwrapped) lines, exactly as
/// rendered. Shared by `draw_transcript` and the off-screen selection
/// extraction so both wrap identically.
fn build_transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<LineKind> = None;
    for entry in &app.transcript {
        if spacer_before(prev, entry.kind) {
            lines.push(Line::from(""));
        }
        prev = Some(entry.kind);
        let (prefix, style) = style_for(entry.kind);
        // Render command output through the ANSI parser (preserves colors).
        if entry.kind == LineKind::Output {
            if let Ok(text) = entry.text.clone().into_text() {
                for mut l in text.lines {
                    l.spans.insert(0, Span::raw("  "));
                    lines.push(l);
                }
                continue;
            }
        }
        for (i, raw) in entry.text.lines().enumerate() {
            let text = if i == 0 {
                format!("{prefix}{raw}")
            } else {
                format!("{}{raw}", " ".repeat(prefix.chars().count()))
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    // Dimmed "thinking" (reasoning) stream, shown above the answer while it
    // streams and cleared once the response commits.
    if !app.reasoning.is_empty() {
        if spacer_before(prev, LineKind::Agent) {
            lines.push(Line::from(""));
        }
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        for (i, raw) in app.reasoning.lines().enumerate() {
            let text = if i == 0 {
                format!("💭 {raw}")
            } else {
                format!("   {raw}")
            };
            lines.push(Line::from(Span::styled(text, dim)));
        }
        prev = Some(LineKind::Agent);
    }
    if !app.streaming.is_empty() {
        if spacer_before(prev, LineKind::Agent) {
            lines.push(Line::from(""));
        }
        let style = style_for(LineKind::Agent).1;
        for raw in app.streaming.lines() {
            lines.push(Line::from(Span::styled(raw.to_string(), style)));
        }
    }
    lines
}

fn draw_transcript(f: &mut Frame, app: &App, area: Rect) {
    let lines = build_transcript_lines(app);

    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    // Record the inner text rect so the event loop can hit-test mouse drags
    // against the transcript only (not the borders or sidebars).
    let text_rect = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    app.transcript_area.set(text_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    // Build the paragraph first so we can ask ratatui for the *exact* wrapped
    // line count (its own word-wrapper) — a char-width estimate undercounts when
    // lines wrap, which leaves `follow` short and hides the newest lines under
    // the input. `line_count(inner_w)` wraps to that width and adds the block's
    // 2 border rows, so subtract them to get content rows.
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    let total = para.line_count(inner_w as u16).saturating_sub(2);
    let max_scroll = total.saturating_sub(inner_h);
    app.max_scroll.set(max_scroll);
    let offset_top = if app.follow {
        max_scroll
    } else {
        app.scroll_top.min(max_scroll)
    }
    .min(u16::MAX as usize) as u16;
    // Record the wrapped-line offset so the event loop can map screen rows to
    // logical positions (and selections can span the scrollback).
    app.scroll_offset.set(offset_top as usize);

    let title = if !app.follow && (offset_top as usize) < max_scroll {
        format!(" {}  ▲ scrollback · Shift+End to follow ", app.title)
    } else {
        format!(" {} ", app.title)
    };
    let para = para.block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(title),
    );
    f.render_widget(para.scroll((offset_top, 0)), area);

    // Scrollbar on the right edge when content overflows. ratatui bottoms the
    // thumb when position == content_length - 1, so content_length is the count
    // of scroll *positions* (0..=max_scroll), not the total line count.
    if max_scroll > 0 {
        let mut sb_state = ratatui::widgets::ScrollbarState::new(max_scroll + 1)
            .viewport_content_length(inner_h)
            .position(offset_top as usize);
        let sb =
            ratatui::widgets::Scrollbar::new(ratatui::widgets::ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::default().fg(Color::DarkGray));
        f.render_stateful_widget(sb, area, &mut sb_state);
    }

    // Paint the selection highlight over the rendered text: for each visible
    // screen row, map it to its logical wrapped-line and highlight the selected
    // columns (if any).
    if let Some(sel) = &app.selection {
        let inner_w = text_rect.width;
        let buf = f.buffer_mut();
        for sy in 0..text_rect.height {
            let line = offset_top as usize + sy as usize;
            if let Some((x0, x1)) = sel.cols_on(line, inner_w) {
                for x in x0..=x1 {
                    let cell = &mut buf[(text_rect.x + x, text_rect.y + sy)];
                    cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

/// A consistent rounded side/utility panel.
fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Gray),
        ))
}

/// Color a `verdict label (reason)` activity line by its leading verdict word.
fn activity_line(raw: &str) -> Line<'static> {
    let (verdict, rest) = raw.split_once(' ').unwrap_or((raw, ""));
    let vstyle = match verdict {
        "allow" => Style::default().fg(Color::Green),
        "deny" => Style::default().fg(Color::Red),
        "ask" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    };
    Line::from(vec![
        Span::styled(format!("{verdict} "), vstyle),
        Span::styled(rest.to_string(), Style::default().fg(Color::Gray)),
    ])
}

fn draw_activity(f: &mut Frame, app: &App, area: Rect) {
    let inner = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if app.activity.is_empty() {
        vec![Line::from(Span::styled(
            "no network activity yet",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        let start = app.activity.len().saturating_sub(inner);
        app.activity[start..]
            .iter()
            .map(|l| activity_line(l))
            .collect()
    };
    let para = Paragraph::new(lines)
        .block(panel("network"))
        .wrap(Wrap { trim: true });
    f.render_widget(para, area);
}

fn draw_plan(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .plan
        .iter()
        .map(|(step, status)| {
            let (mark, color) = match status.as_str() {
                "done" => ("✓", Color::Green),
                "in_progress" => ("▸", Color::Yellow),
                _ => ("·", Color::DarkGray),
            };
            let step_style = match status.as_str() {
                "done" => Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT),
                "in_progress" => Style::default().fg(Color::White),
                _ => Style::default().fg(Color::Gray),
            };
            Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color)),
                Span::styled(step.clone(), step_style),
            ])
        })
        .collect();
    let done = app.plan.iter().filter(|(_, s)| s == "done").count();
    let title = format!("plan {done}/{}", app.plan.len());
    let para = Paragraph::new(lines)
        .block(panel(&title))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn draw_processes(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = if app.processes.is_empty() {
        vec![Line::from(Span::styled(
            "no managed processes",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.processes
            .iter()
            .map(|(n, s)| {
                Line::from(vec![
                    Span::styled(format!("{n:<14} "), Style::default().fg(Color::White)),
                    Span::styled(s.clone(), Style::default().fg(Color::Green)),
                ])
            })
            .collect()
    };
    let para = Paragraph::new(lines).block(panel("processes"));
    f.render_widget(para, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let mode = match &app.mode {
        Mode::Running => "running",
        Mode::Idle => "ready",
        Mode::AwaitingInput(_) => "awaiting input",
        Mode::AwaitingChoice => "awaiting choice",
        Mode::Approval(_) => "approval",
        Mode::Paused => "paused",
        Mode::ModelPicker => "models",
        Mode::ModelForm => "models",
        Mode::Done => "done",
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(area);
    if app.mode == Mode::Running {
        // Animate via a throwaway clone (draw takes &App).
        let mut ts = app.throbber.clone();
        f.render_stateful_widget(Throbber::default(), cols[0], &mut ts);
    }
    let bar = Style::default().bg(Color::Blue).fg(Color::White);
    // Right side: a blocked flag, the running token estimate, then the diff.
    let mut segs: Vec<String> = Vec::new();
    if app.blocked.is_some() {
        segs.push("⏸ blocked".to_string());
    }
    if app.tokens_in > 0 || app.tokens_out > 0 {
        let mut seg = format!(
            "~{}↑ {}↓",
            fmt_count(app.tokens_in),
            fmt_count(app.tokens_out)
        );
        if app.cost_usd > 0.0 {
            seg.push_str(&format!(" ${}", fmt_cost(app.cost_usd)));
        }
        segs.push(seg);
    }
    if !app.diff.is_empty() {
        segs.push(app.diff.clone());
    }
    let right_text = segs.join("   ");
    let (left, right) = if right_text.is_empty() {
        (cols[1], None)
    } else {
        let w = right_text.chars().count() as u16 + 1;
        let s = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(w)])
            .split(cols[1]);
        (s[0], Some(s[1]))
    };
    // While a shell command runs, the left segment becomes a live tail:
    // elapsed time + the latest output line (the spinner is in cols[0]).
    let text = match &app.running {
        Some(r) => {
            let tail = r.last.trim();
            let body = if tail.is_empty() { &r.cmd } else { tail };
            format!(" exec {}s › {body}", r.elapsed_secs)
        }
        None => format!(" {mode} — {}", app.status),
    };
    f.render_widget(Paragraph::new(text).style(bar), left);
    if let Some(right) = right {
        f.render_widget(Paragraph::new(format!("{right_text} ")).style(bar), right);
    }
}

/// Compact human count: `980`, `12.3k`, `45k`, `1.2M`.
fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// USD to two decimals, but a tiny nonzero spend shows `<0.01` rather than `0.00`.
fn fmt_cost(usd: f64) -> String {
    if usd > 0.0 && usd < 0.005 {
        "<0.01".to_string()
    } else {
        format!("{usd:.2}")
    }
}

/// The slash-command/skill autocomplete popup, anchored just above the input.
fn draw_completions(f: &mut Frame, app: &App, input_area: Rect) {
    let Some(cs) = &app.completion else { return };
    if cs.items.is_empty() {
        return;
    }
    const MAX_ROWS: usize = 8;
    let shown = cs.items.len().min(MAX_ROWS);
    // Scroll a window so the selected item stays visible.
    let start = if cs.selected < MAX_ROWS {
        0
    } else {
        cs.selected - MAX_ROWS + 1
    };
    let widest = cs
        .items
        .iter()
        .map(|c| c.value.chars().count() + c.hint.chars().count() + 5)
        .max()
        .unwrap_or(24);
    let width = (widest as u16)
        .max(24)
        .min(input_area.width.saturating_sub(2).max(24));
    let height = shown as u16 + 2;
    let rect = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };
    let lines: Vec<Line> = cs.items[start..start + shown]
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let selected = start + i == cs.selected;
            let name = Style::default().fg(if selected { Color::Black } else { Color::Cyan });
            let hint = Style::default().fg(if selected {
                Color::Black
            } else {
                Color::DarkGray
            });
            let row = Line::from(vec![
                Span::styled(format!("/{}", c.value), name),
                Span::styled(format!("  {}", c.hint), hint),
            ]);
            if selected {
                row.style(Style::default().bg(Color::Cyan))
            } else {
                row
            }
        })
        .collect();
    let title = format!(
        " {} match{} · Tab ",
        cs.items.len(),
        if cs.items.len() == 1 { "" } else { "es" }
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(title, Style::default().fg(Color::DarkGray)));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.mode {
        Mode::Done => "session finished — press q to quit",
        Mode::AwaitingInput(_) => "type your answer · Enter submits",
        Mode::Idle => "Enter send · Shift+Enter newline · ↑↓ history · /help · Ctrl-C menu",
        _ => "Enter send · Shift+Enter newline · /help · Ctrl-C interrupt",
    };
    let accent = if app.mode == Mode::Idle {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(
            format!(" {hint} "),
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(&app.textarea, inner);
}

/// A multiple-choice question: the prompt, a selectable option list, and a
/// free-text "other" line reflecting what's been typed.
fn draw_choice(f: &mut Frame, area: Rect, c: &Choice, typed: &str) {
    let w = area.width.saturating_sub(8).min(70);
    let h = (c.options.len() as u16 + 6).min(area.height).max(7);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(Span::styled(
            " Question ",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().fg(Color::Magenta));

    let typing = !typed.trim().is_empty();
    let mut lines = vec![Line::from(c.question.clone()), Line::from("")];
    for (i, opt) in c.options.iter().enumerate() {
        let selected = !typing && i == c.selected;
        let marker = if selected { "▸" } else { " " };
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(
            format!("{marker} {}. {opt}", i + 1),
            style,
        )));
    }
    let other = if typing {
        Line::from(vec![
            Span::styled("▸ other: ", Style::default().fg(Color::White)),
            Span::styled(typed.to_string(), Style::default().fg(Color::White)),
        ])
    } else {
        Line::from(Span::styled(
            "  (or type a custom answer)",
            Style::default().add_modifier(Modifier::DIM),
        ))
    };
    lines.push(other);
    lines.push(Line::from(Span::styled(
        "↑↓ select · 1-9 pick · Enter choose · type for other",
        Style::default().add_modifier(Modifier::DIM),
    )));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, rect);
}

fn draw_modal(f: &mut Frame, area: Rect, title: &str, body: &str, footer: &str) {
    let w = area.width.saturating_sub(8).min(70);
    let h = 7u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().fg(Color::Magenta));
    let text = vec![
        Line::from(body.to_string()),
        Line::from(""),
        Line::from(Span::styled(
            footer.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];
    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
    f.render_widget(para, rect);
}

#[cfg(test)]
mod tests {
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
    fn snapshot_welcome_screen() {
        let mut app = App::new("cowboy · 20260614-abcd");
        for l in [
            "Welcome to cowboy — the agent runs sandboxed in Docker.",
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
    fn snapshot_paused_menu() {
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "do work");
        app.mode = Mode::Paused;
        insta::assert_snapshot!(render(&app));
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
