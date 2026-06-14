//! Renderable TUI state and drawing. The CLI owns the event loop and feeds
//! this `App`; here we keep only state + a pure `draw` so rendering is
//! snapshot-testable with `ratatui::backend::TestBackend`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

/// Kind of a transcript line (drives color/prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Agent,
    Command,
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
    /// Agent is working.
    Running,
    /// Waiting for the user to answer an `ask_user` question.
    AwaitingInput(String),
    /// A network approval prompt is showing.
    Approval(String),
    /// Paused via interrupt; showing the menu.
    Paused,
    /// Session finished.
    Done,
}

/// Full renderable TUI state.
pub struct App {
    pub title: String,
    pub status: String,
    pub transcript: Vec<TranscriptLine>,
    /// In-progress streamed agent text (not yet committed to the transcript).
    pub streaming: String,
    pub input: String,
    pub mode: Mode,
    pub spinner_frame: usize,
}

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

impl App {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            status: "ready".into(),
            transcript: Vec::new(),
            streaming: String::new(),
            input: String::new(),
            mode: Mode::Running,
            spinner_frame: 0,
        }
    }

    pub fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.transcript.push(TranscriptLine {
            kind,
            text: text.into(),
        });
    }

    /// Append streamed agent text.
    pub fn stream(&mut self, text: &str) {
        self.streaming.push_str(text);
    }

    /// Commit any streamed text to the transcript as an Agent line.
    pub fn commit_stream(&mut self) {
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.push(LineKind::Agent, text);
        }
    }

    pub fn tick(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
    }
}

fn style_for(kind: LineKind) -> (&'static str, Style) {
    match kind {
        LineKind::User => (
            "you ",
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
        LineKind::Output => ("", Style::default().fg(Color::Gray)),
        LineKind::Final => (
            "✓ ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        LineKind::Notice => ("", Style::default().fg(Color::DarkGray)),
        LineKind::Error => ("! ", Style::default().fg(Color::Red)),
    }
}

/// Draw the whole UI.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // transcript
            Constraint::Length(1), // status bar
            Constraint::Length(3), // input
        ])
        .split(area);

    draw_transcript(f, app, chunks[0]);
    draw_status(f, app, chunks[1]);
    draw_input(f, app, chunks[2]);

    match &app.mode {
        Mode::AwaitingInput(q) => draw_modal(f, area, "Question", q, &app.input),
        Mode::Approval(p) => draw_modal(
            f,
            area,
            "Network approval",
            p,
            "[o]nce [s]ession [p]roject [g]lobal [d]eny",
        ),
        Mode::Paused => draw_modal(
            f,
            area,
            "Paused",
            "Agent paused.",
            "[r]esume  [i]nstruct  [k]ill command  [e]nd session",
        ),
        _ => {}
    }
}

fn draw_transcript(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    for entry in &app.transcript {
        let (prefix, style) = style_for(entry.kind);
        for (i, raw) in entry.text.lines().enumerate() {
            let text = if i == 0 {
                format!("{prefix}{raw}")
            } else {
                raw.to_string()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    if !app.streaming.is_empty() {
        let style = style_for(LineKind::Agent).1;
        for raw in app.streaming.lines() {
            lines.push(Line::from(Span::styled(raw.to_string(), style)));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", app.title));
    // Show the tail that fits.
    let inner_height = area.height.saturating_sub(2) as usize;
    let start = lines.len().saturating_sub(inner_height);
    let visible: Vec<Line> = lines.into_iter().skip(start).collect();
    let para = Paragraph::new(visible)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let spin = if app.mode == Mode::Running {
        SPINNER[app.spinner_frame]
    } else {
        " "
    };
    let mode = match &app.mode {
        Mode::Running => "running",
        Mode::AwaitingInput(_) => "awaiting input",
        Mode::Approval(_) => "approval",
        Mode::Paused => "paused",
        Mode::Done => "done",
    };
    let text = format!(" {spin} {mode} — {}", app.status);
    let para = Paragraph::new(text).style(Style::default().bg(Color::Blue).fg(Color::White));
    f.render_widget(para, area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.mode {
        Mode::Done => "session finished — press q to quit",
        Mode::AwaitingInput(_) => "type your answer, Enter to submit",
        _ => "type a message · Enter send · Ctrl-C interrupt",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {hint} "));
    let para = Paragraph::new(format!("> {}", app.input)).block(block);
    f.render_widget(para, area);
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
        .title(format!(" {title} "))
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

    /// Render the app to a fixed-size buffer and return it as text.
    fn render(app: &App) -> String {
        let mut term = Terminal::new(TestBackend::new(60, 16)).unwrap();
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
    fn snapshot_running_transcript() {
        let mut app = App::new("cowboy");
        app.status = "exec: cargo test".into();
        app.push(LineKind::User, "fix the failing test");
        app.push(LineKind::Command, "cargo test");
        app.push(LineKind::Output, "test result: FAILED");
        app.stream("Looking at the failure...");
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn snapshot_approval_modal() {
        let mut app = App::new("cowboy");
        app.push(LineKind::User, "build the project");
        app.mode = Mode::Approval("github.com:443 (HTTPS)".into());
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn snapshot_done() {
        let mut app = App::new("cowboy");
        app.push(LineKind::Final, "Implemented the fix; tests pass.");
        app.mode = Mode::Done;
        app.status = "finished".into();
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
}
