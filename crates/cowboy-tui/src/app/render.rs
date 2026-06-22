//! All transcript/overlay rendering for the TUI: the `draw` entry point, the
//! per-pane `draw_*` helpers, Markdown/diff line building, and the OSC 8
//! hyperlink overlay. Split out of `app/mod.rs` so that file holds the view
//! *state* and this one holds how it's painted. A child module of `app`, so it
//! shares `App`'s private fields.

use super::*;

pub(super) fn style_for(kind: LineKind) -> (&'static str, Style) {
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
        LineKind::Diff => ("", Style::default().fg(Color::Gray)),
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

/// Render a unified diff with per-line coloring: additions green, deletions
/// red, hunk headers cyan, file headers bold, context dimmed. Each line carries
/// a full-width-ish background tint so the diff reads as a contiguous block.
pub(super) fn render_diff(text: &str) -> Vec<Line<'static>> {
    const ADD_FG: Color = Color::Rgb(166, 227, 161);
    const ADD_BG: Color = Color::Rgb(28, 42, 30);
    const DEL_FG: Color = Color::Rgb(243, 139, 168);
    const DEL_BG: Color = Color::Rgb(45, 28, 32);
    const HUNK_FG: Color = Color::Rgb(137, 180, 250);

    text.lines()
        .map(|raw| {
            let (style, prefix) = if raw.starts_with("+++") || raw.starts_with("---") {
                (
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                    "  ",
                )
            } else if raw.starts_with("@@") {
                (
                    Style::default().fg(HUNK_FG).add_modifier(Modifier::BOLD),
                    "  ",
                )
            } else if raw.starts_with('+') {
                (Style::default().fg(ADD_FG).bg(ADD_BG), "  ")
            } else if raw.starts_with('-') {
                (Style::default().fg(DEL_FG).bg(DEL_BG), "  ")
            } else if raw.starts_with("diff ") || raw.starts_with("index ") {
                (Style::default().fg(Color::DarkGray), "  ")
            } else {
                (Style::default().fg(Color::Gray), "  ")
            };
            Line::from(vec![
                Span::raw(prefix),
                Span::styled(raw.to_string(), style),
            ])
        })
        .collect()
}

/// Insert a blank spacer before this entry when it starts a new "block" so the
/// transcript breathes (e.g. before a user turn or the final summary).
pub(super) fn spacer_before(prev: Option<LineKind>, cur: LineKind) -> bool {
    let Some(prev) = prev else { return false };
    if prev == cur && matches!(cur, LineKind::Output | LineKind::Command | LineKind::Tool) {
        return false;
    }
    matches!(
        cur,
        LineKind::User
            | LineKind::Final
            | LineKind::Command
            | LineKind::Tool
            | LineKind::Agent
            | LineKind::Diff
    ) && prev != LineKind::Banner
}

/// Draw the whole UI.
pub fn draw(f: &mut Frame, app: &App) {
    // Watching a subagent: render its nested transcript full-screen instead of the
    // main session, with a header + a return hint.
    if app.mode == Mode::WatchingSubagent {
        if let Some(sub) = app.watching.as_deref() {
            draw_watch(f, app, sub);
            return;
        }
    }
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
        draw_background(f, app, side[1]);
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
        draw_background(f, app, side[2]);
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
            "Network request",
            &format!(
                "{p}\n\n\
                 o  once — just this request\n\
                 s  session — every request here until this session ends\n\
                 p  project — always allow here (saved for this repo)\n\
                 g  global — always allow everywhere\n\
                 d  deny",
            ),
            "press a key  ·  Esc = deny",
        ),
        Mode::Paused => draw_modal(
            f,
            area,
            "Paused",
            "r  resume\n\
             i  instruct — stop this turn and redirect (history kept)\n\
             k  kill — stop the running command/turn only\n\
             w  watch — open a subagent's live output\n\
             d  detach — leave it running in the background, exit\n\
             e  end — finish the session",
            "press a key  ·  Esc = resume",
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

/// Render a watched subagent's nested transcript full-screen, with a header
/// naming it and a footer hint (Esc to return, `w` to cycle).
fn draw_watch(f: &mut Frame, app: &App, sub: &App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    let header = Line::from(vec![
        Span::styled(
            " 👁 watching ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            app.watch_label.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  [{}]", app.watch_id),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(header), rows[0]);
    draw_transcript(f, sub, rows[1]);
    let footer = Line::from(vec![
        Span::styled(
            format!(" {} ", sub.status),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            "·  Esc back  ·  w next subagent",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(footer), rows[2]);
}

/// Centered rect `w`×`h` (clamped to `area`), cleared for an overlay.
pub(super) fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

pub(super) fn draw_model_picker(f: &mut Frame, area: Rect, p: &ModelPicker) {
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

pub(super) fn draw_model_form(f: &mut Frame, area: Rect, form: &ModelForm) {
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
pub(super) fn field_line(label: &str, value: &str, focused: bool) -> Line<'static> {
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
pub(super) fn trunc(s: &str, max: usize) -> String {
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
pub(super) fn build_transcript_lines(app: &App) -> Vec<Line<'static>> {
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
        // A captured file diff renders with +/- coloring.
        if entry.kind == LineKind::Diff {
            for l in render_diff(&entry.text) {
                lines.push(l);
            }
            continue;
        }
        // Agent/Final prose is Markdown: headings, emphasis, lists, links, and
        // syntax-highlighted code blocks. A leading marker keeps the speaker cue.
        if matches!(entry.kind, LineKind::Agent | LineKind::Final)
            && markdown::looks_like_markdown(&entry.text)
        {
            let mut md = markdown::render(&entry.text, style);
            if !prefix.is_empty() {
                if let Some(first) = md.first_mut() {
                    first
                        .spans
                        .insert(0, Span::styled(prefix.to_string(), style));
                }
            }
            lines.append(&mut md);
            continue;
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
        // Render the in-progress answer through the same Markdown path as
        // committed text, so formatting resolves *live* as tokens stream in
        // (headings bold, lists indent, code blocks start highlighting) instead
        // of snapping into shape only when the turn commits. pulldown-cmark
        // renders partial/unterminated constructs gracefully and reflows.
        if markdown::looks_like_markdown(&app.streaming) {
            lines.extend(markdown::render(&app.streaming, style));
        } else {
            for raw in app.streaming.lines() {
                lines.push(Line::from(Span::styled(raw.to_string(), style)));
            }
        }
    }
    lines
}

/// Build the transcript lines, memoized by content version: re-parsing Markdown
/// and re-running syntax highlighting on every frame (throbber ticks redraw
/// ~10×/s even when nothing changed) is wasteful, so we rebuild only when the
/// content version advanced since the last build. Selection highlighting is
/// painted onto the rendered buffer afterwards, so it doesn't affect this cache.
pub(super) fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let ver = app.content_ver.get();
    if let Some((cached_ver, lines)) = app.line_cache.borrow().as_ref() {
        if *cached_ver == ver {
            return lines.clone();
        }
    }
    let lines = build_transcript_lines(app);
    *app.line_cache.borrow_mut() = Some((ver, lines.clone()));
    lines
}

/// One rendered link: an absolute wrapped-row + inclusive column span carrying a
/// URL, used to overlay an OSC 8 hyperlink onto the rendered buffer.
#[derive(Clone)]
pub(super) struct LinkHit {
    pub(super) row: usize,
    pub(super) col0: u16,
    pub(super) col1: u16,
    pub(super) url: String,
}

/// Every link URL in the transcript (and the in-progress stream), in the order
/// their labels render — so they zip onto the link cells found in the buffer.
pub(super) fn transcript_link_urls(app: &App) -> Vec<String> {
    let mut urls = Vec::new();
    for e in &app.transcript {
        if matches!(e.kind, LineKind::Agent | LineKind::Final)
            && markdown::looks_like_markdown(&e.text)
        {
            urls.extend(markdown::link_urls(&e.text));
        }
    }
    if !app.streaming.is_empty() && markdown::looks_like_markdown(&app.streaming) {
        urls.extend(markdown::link_urls(&app.streaming));
    }
    urls
}

/// Locate link placements by rendering the transcript off-screen at `width` and
/// scanning for runs of link-styled cells, zipping them onto the document-order
/// URL list. Cached by (content version, width); skipped entirely when there are
/// no links (the common case), so it costs nothing for link-free transcripts.
pub(super) fn link_hits(app: &App, width: u16) -> Vec<LinkHit> {
    let ver = app.content_ver.get();
    if let Some((v, w, hits)) = app.link_cache.borrow().as_ref() {
        if *v == ver && *w == width {
            return hits.clone();
        }
    }
    let urls = transcript_link_urls(app);
    let hits = if urls.is_empty() || width == 0 {
        Vec::new()
    } else {
        compute_link_hits(&transcript_lines(app), width, &urls)
    };
    *app.link_cache.borrow_mut() = Some((ver, width, hits.clone()));
    hits
}

pub(super) fn is_link_cell(cell: &ratatui::buffer::Cell) -> bool {
    cell.fg == markdown::LINK_FG && cell.modifier.contains(Modifier::UNDERLINED)
}

pub(super) fn compute_link_hits(
    lines: &[Line<'static>],
    width: u16,
    urls: &[String],
) -> Vec<LinkHit> {
    let para = Paragraph::new(lines.to_vec()).wrap(Wrap { trim: false });
    let total = para.line_count(width).min(u16::MAX as usize) as u16;
    if total == 0 {
        return Vec::new();
    }
    let mut buf = Buffer::empty(Rect::new(0, 0, width, total));
    para.render(buf.area, &mut buf);

    let mut hits: Vec<LinkHit> = Vec::new();
    let mut idx = 0;
    // A link wrapped across rows yields a run at col 0 right after a run that
    // reached the right edge; treat that as a continuation of the same URL.
    let mut prev_ended_at_edge = false;
    for y in 0..total {
        let mut x = 0u16;
        let mut first_run = true;
        let mut last_end: Option<u16> = None;
        while x < width {
            if is_link_cell(&buf[(x, y)]) {
                let col0 = x;
                while x < width && is_link_cell(&buf[(x, y)]) {
                    x += 1;
                }
                let col1 = x - 1;
                let continuation = first_run && col0 == 0 && prev_ended_at_edge;
                let url = if continuation {
                    hits.last().map(|h| h.url.clone()).unwrap_or_default()
                } else {
                    let u = urls.get(idx).cloned().unwrap_or_default();
                    idx += 1;
                    u
                };
                if !url.is_empty() {
                    hits.push(LinkHit {
                        row: y as usize,
                        col0,
                        col1,
                        url,
                    });
                }
                last_end = Some(col1);
                first_run = false;
            } else {
                x += 1;
            }
        }
        prev_ended_at_edge = last_end == Some(width - 1);
    }
    hits
}

/// Overlay OSC 8 hyperlinks onto the rendered transcript: for each visible link
/// placement, rewrite its cells' symbols to wrap the label text in the escape
/// (`ESC ] 8 ; ; URL BEL text ESC ] 8 ; ; BEL`). Following ratatui's own
/// hyperlink example, the text is emitted in 2-char chunks placed every 2 cells
/// to work around ratatui's escape-width miscalculation (issue #902).
pub(super) fn overlay_hyperlinks(buf: &mut Buffer, app: &App, text_rect: Rect, offset_top: usize) {
    let hits = link_hits(app, text_rect.width);
    let top = offset_top;
    let bottom = offset_top + text_rect.height as usize;
    for h in hits.iter().filter(|h| h.row >= top && h.row < bottom) {
        let sy = (h.row - top) as u16;
        let y = text_rect.y + sy;
        // Reconstruct the label from the already-rendered cells.
        let label: String = (h.col0..=h.col1)
            .map(|x| buf[(text_rect.x + x, y)].symbol().to_string())
            .collect();
        let chars: Vec<char> = label.chars().collect();
        for (i, chunk) in chars.chunks(2).enumerate() {
            let piece: String = chunk.iter().collect();
            let cx = text_rect.x + h.col0 + (i as u16) * 2;
            if cx >= text_rect.x + text_rect.width {
                break;
            }
            let hyper = format!("\x1b]8;;{}\x07{}\x1b]8;;\x07", h.url, piece);
            buf[(cx, y)].set_symbol(&hyper);
        }
    }
}

pub(super) fn draw_transcript(f: &mut Frame, app: &App, area: Rect) {
    let lines = transcript_lines(app);

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

    // Overlay OSC 8 hyperlinks onto link cells (clickable in capable terminals).
    overlay_hyperlinks(f.buffer_mut(), app, text_rect, offset_top as usize);

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
pub(super) fn panel(title: &str) -> Block<'_> {
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
pub(super) fn activity_line(e: &ActivityEntry) -> Line<'static> {
    let vstyle = match e.verdict.as_str() {
        "allow" => Style::default().fg(Color::Green),
        "deny" => Style::default().fg(Color::Red),
        "ask" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    };
    let mut spans = vec![
        Span::styled(format!("{} ", e.verdict), vstyle),
        Span::styled(e.host.clone(), Style::default().fg(Color::Gray)),
    ];
    if e.count > 1 {
        spans.push(Span::styled(
            format!(" ({}x)", e.count),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

pub(super) fn draw_activity(f: &mut Frame, app: &App, area: Rect) {
    let inner_w = area.width.saturating_sub(2).max(1);
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if app.activity.is_empty() {
        vec![Line::from(Span::styled(
            "no network activity yet",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.activity.iter().map(activity_line).collect()
    };
    let para = Paragraph::new(lines)
        .block(panel("network"))
        .wrap(Wrap { trim: true });
    // Pin to the bottom: scroll past everything above the last `inner_h` wrapped
    // rows so the newest activity is always visible (rows can wrap). line_count
    // includes the block's 2 border rows, so subtract them.
    let total = para.line_count(inner_w).saturating_sub(2);
    let scroll = total.saturating_sub(inner_h).min(u16::MAX as usize) as u16;
    f.render_widget(para.scroll((scroll, 0)), area);
}

pub(super) fn draw_plan(f: &mut Frame, app: &App, area: Rect) {
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

/// One combined pane for background activity: spawned crew subagents and any
/// managed (`cowboy proc`) processes.
pub(super) fn draw_background(f: &mut Frame, app: &App, area: Rect) {
    let short = |m: &str| m.rsplit('/').next().unwrap_or(m).to_string();
    let mut lines: Vec<Line> = Vec::new();
    // Crew subagents first (most active background work).
    for m in &app.crew {
        let (mark, color) = match m.status {
            CrewStatus::Running => ("⟳", Color::Yellow),
            CrewStatus::Done => ("✓", Color::Green),
            CrewStatus::Failed => ("✗", Color::Red),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(color)),
            Span::styled(
                format!("{:<10} ", m.label),
                Style::default().fg(Color::White),
            ),
            Span::styled(short(&m.model), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!(" {}s", m.elapsed_secs),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    // Then managed processes.
    for (n, s) in &app.processes {
        lines.push(Line::from(vec![
            Span::styled(format!("{n:<12} "), Style::default().fg(Color::White)),
            Span::styled(s.clone(), Style::default().fg(Color::Green)),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "no background activity",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let para = Paragraph::new(lines).block(panel("background"));
    f.render_widget(para, area);
}

pub(super) fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let mode = match &app.mode {
        Mode::Running => "running",
        Mode::Idle => "ready",
        // These three are deliberate "over to you" pauses, not stalls — the agent
        // is parked waiting on your answer and will pick up the moment you reply.
        Mode::AwaitingInput(_) => "your turn — answer above",
        Mode::AwaitingChoice => "your turn — pick above",
        Mode::Approval(_) => "paused for you — your call",
        Mode::Paused => "paused",
        Mode::ModelPicker => "models",
        Mode::ModelForm => "models",
        Mode::WatchingSubagent => "watching",
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
        // A shell command is running: live tail (elapsed + latest output line).
        Some(r) => {
            let tail = r.last.trim();
            let body = if tail.is_empty() { &r.cmd } else { tail };
            format!(" exec {}s › {body}", r.elapsed_secs)
        }
        // Plan mode: keep "edits are blocked, /go to execute" visible the whole
        // time you're planning, not just right after /plan.
        None if app.plan_mode && app.mode == Mode::Running => {
            format!(" 🧭 planning {}s… (edits blocked)", app.turn_elapsed_secs())
        }
        None if app.plan_mode => " 🧭 plan mode — review, then /go to execute".to_string(),
        // Model turn in flight with no command: a "thinking Ns" heartbeat so the
        // wait never feels like dead air.
        None if app.mode == Mode::Running => {
            format!(" thinking {}s…", app.turn_elapsed_secs())
        }
        // Idle with uncommitted changes: quietly hand the user their next move.
        None if app.mode == Mode::Idle && !app.diff.is_empty() => {
            " ready · /diff to review, then commit".to_string()
        }
        None => format!(" {mode} — {}", app.status),
    };
    f.render_widget(Paragraph::new(text).style(bar), left);
    if let Some(right) = right {
        f.render_widget(Paragraph::new(format!("{right_text} ")).style(bar), right);
    }
}

/// Compact human count: `980`, `12.3k`, `45k`, `1.2M`.
pub(super) fn fmt_count(n: u64) -> String {
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
pub(super) fn fmt_cost(usd: f64) -> String {
    if usd > 0.0 && usd < 0.005 {
        "<0.01".to_string()
    } else {
        format!("{usd:.2}")
    }
}

/// The slash-command/skill autocomplete popup, anchored just above the input.
pub(super) fn draw_completions(f: &mut Frame, app: &App, input_area: Rect) {
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

pub(super) fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.mode {
        Mode::Done => "session finished — press q to quit",
        Mode::AwaitingInput(_) => "type your answer · Enter submits",
        Mode::Idle => {
            "Enter send · Shift+Enter newline · ↑↓ history · drag+y copy · /help · Ctrl-C menu"
        }
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
pub(super) fn draw_choice(f: &mut Frame, area: Rect, c: &Choice, typed: &str) {
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

pub(super) fn draw_modal(f: &mut Frame, area: Rect, title: &str, body: &str, footer: &str) {
    // `body` may be multi-line (e.g. a key legend); size the modal to fit it.
    let body_lines: Vec<&str> = body.lines().collect();
    let w = area.width.saturating_sub(8).min(72);
    // borders (2) + body + blank separator (1) + footer (1).
    let h = (body_lines.len() as u16 + 4).min(area.height);
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
    let mut text: Vec<Line> = body_lines
        .iter()
        .map(|l| Line::from(l.to_string()))
        .collect();
    text.push(Line::from(""));
    text.push(Line::from(Span::styled(
        footer.to_string(),
        Style::default().add_modifier(Modifier::DIM),
    )));
    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
    f.render_widget(para, rect);
}
