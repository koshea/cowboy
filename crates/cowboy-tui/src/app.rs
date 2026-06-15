//! Renderable TUI state and drawing. The CLI owns the event loop and feeds
//! this `App`; here we keep state + a pure `draw` so rendering is
//! snapshot-testable with `ratatui::backend::TestBackend`.

use ansi_to_tui::IntoText;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use throbber_widgets_tui::{Throbber, ThrobberState};
use tui_textarea::TextArea;

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

/// Full renderable TUI state.
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
    /// Input editor (multi-line, cursor) via tui-textarea.
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
    /// Inner text rect of the transcript, captured each frame so the event loop
    /// can hit-test mouse coordinates against the transcript only.
    pub transcript_area: std::cell::Cell<Rect>,
    /// Model-catalogue picker state (set while `mode == ModelPicker`).
    pub model_picker: Option<ModelPicker>,
    /// New-model config form state (set while `mode == ModelForm`).
    pub model_form: Option<ModelForm>,
}

/// A mouse text selection, in absolute terminal coordinates (col, row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag began.
    pub anchor: (u16, u16),
    /// Current drag position.
    pub cursor: (u16, u16),
}

/// Per-row selected column spans `(row, x_start, x_end)` (inclusive), in linear
/// reading order, clamped to `rect`.
fn selection_spans(rect: Rect, sel: &Selection) -> Vec<(u16, u16, u16)> {
    // Order the endpoints by (row, col) so selection reads top-to-bottom.
    let (start, end) = if (sel.anchor.1, sel.anchor.0) <= (sel.cursor.1, sel.cursor.0) {
        (sel.anchor, sel.cursor)
    } else {
        (sel.cursor, sel.anchor)
    };
    let (sx, sy) = start;
    let (ex, ey) = end;
    let left = rect.x;
    let right = rect.right().saturating_sub(1);
    let (top, bot) = (rect.y, rect.bottom().saturating_sub(1));
    let mut spans = Vec::new();
    for y in sy.max(top)..=ey.min(bot) {
        let x0 = if y == sy { sx } else { left }.clamp(left, right);
        let x1 = if y == ey { ex } else { right }.clamp(left, right);
        if x1 >= x0 {
            spans.push((y, x0, x1));
        }
    }
    spans
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
            textarea: TextArea::default(),
            mode: Mode::Running,
            throbber: ThrobberState::default(),
            follow: true,
            scroll_top: 0,
            max_scroll: std::cell::Cell::new(0),
            selection: None,
            transcript_area: std::cell::Cell::new(Rect::ZERO),
            model_picker: None,
            model_form: None,
        }
    }

    /// Begin a selection at an absolute screen position, but only if it lands in
    /// the transcript (clicks elsewhere just clear any selection).
    pub fn begin_selection(&mut self, col: u16, row: u16) {
        if self.transcript_area.get().contains(Position::new(col, row)) {
            self.selection = Some(Selection {
                anchor: (col, row),
                cursor: (col, row),
            });
        } else {
            self.selection = None;
        }
    }

    /// Extend the active selection, clamping to the transcript rect.
    pub fn drag_selection(&mut self, col: u16, row: u16) {
        let r = self.transcript_area.get();
        if let Some(sel) = &mut self.selection {
            sel.cursor = (
                col.clamp(r.x, r.right().saturating_sub(1)),
                row.clamp(r.y, r.bottom().saturating_sub(1)),
            );
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// Extract the selected text from a rendered `buf`, reading only the
    /// transcript's columns so sidebars never bleed in. Returns `None` for a
    /// bare click (no drag) or an all-whitespace selection.
    pub fn selected_text(&self, buf: &Buffer) -> Option<String> {
        let sel = self.selection?;
        if sel.anchor == sel.cursor {
            return None; // a click, not a drag
        }
        let rect = self.transcript_area.get();
        let lines: Vec<String> = selection_spans(rect, &sel)
            .into_iter()
            .map(|(y, x0, x1)| {
                let mut s = String::new();
                for x in x0..=x1 {
                    s.push_str(buf[(x, y)].symbol());
                }
                s.trim_end().to_string()
            })
            .collect();
        let text = lines.join("\n");
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
        self.transcript.push(TranscriptLine {
            kind,
            text: text.into(),
        });
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
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // main
            Constraint::Length(1), // status bar
            Constraint::Length(3), // input
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

    match &app.mode {
        Mode::AwaitingInput(q) => draw_modal(f, area, "Question", q, "type your answer · Enter"),
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

    let footer = "↑/↓ move · Enter select · Esc cancel · type to filter";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[2],
    );
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

fn draw_transcript(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
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

    let title = if !app.follow && (offset_top as usize) < max_scroll {
        format!(" {}  ▲ scrollback · End to follow ", app.title)
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

    // Paint the selection highlight over the rendered text.
    if let Some(sel) = &app.selection {
        let buf = f.buffer_mut();
        for (y, x0, x1) in selection_spans(text_rect, sel) {
            for x in x0..=x1 {
                let cell = &mut buf[(x, y)];
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
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
    let text = format!(" {mode} — {}", app.status);
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

        // Select the full first text row of the transcript.
        let r = app.transcript_area.get();
        app.selection = Some(Selection {
            anchor: (r.x, r.y),
            cursor: (r.right() - 1, r.y),
        });
        let text = app.selected_text(term.backend().buffer()).unwrap();
        assert!(
            text.contains("hello world from the transcript"),
            "got {text:?}"
        );
        // The sidebar shares the row but lives in other columns — excluded.
        assert!(!text.contains("example.com"), "sidebar leaked: {text:?}");

        // A bare click (no drag) copies nothing.
        app.selection = Some(Selection {
            anchor: (r.x, r.y),
            cursor: (r.x, r.y),
        });
        assert!(app.selected_text(term.backend().buffer()).is_none());
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
