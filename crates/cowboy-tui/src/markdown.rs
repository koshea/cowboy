//! Render Markdown (the agent's answers) into styled ratatui lines: headings,
//! bold/italic/strikethrough, inline code, links, bullet/ordered lists,
//! blockquotes, thematic breaks, and fenced code blocks with `syntect` syntax
//! highlighting. The output is `Vec<Line<'static>>` so it flows through the same
//! word-wrap + selection-extraction path as plain transcript lines.

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::UnicodeWidthStr;

/// Code-block / inline-code accent colors, tuned for a dark terminal.
const CODE_BG: Color = Color::Rgb(38, 42, 54);
const INLINE_CODE_FG: Color = Color::Rgb(231, 197, 152);
const HEADING_FG: Color = Color::Rgb(245, 245, 245);
/// Link text color. Public so the draw layer can detect link cells in the
/// rendered buffer (by `fg == LINK_FG` + underline) and overlay OSC 8 hyperlinks.
pub const LINK_FG: Color = Color::Rgb(120, 170, 255);
const RULE_FG: Color = Color::Rgb(90, 95, 110);

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| {
        let mut ts = ThemeSet::load_defaults();
        ts.themes
            .remove("base16-ocean.dark")
            .or_else(|| ts.themes.remove("Solarized (dark)"))
            .unwrap_or_default()
    })
}

/// True when `text` is worth parsing as Markdown — i.e. it contains a construct
/// our renderer styles. Plain prose with no markup renders identically either
/// way, so we skip the parser for it (cheaper, and avoids surprising reflow).
pub fn looks_like_markdown(text: &str) -> bool {
    text.contains("```")
        || text.contains("**")
        || text.contains("- ")
        || text.contains("* ")
        || text.contains('`')
        || text.contains('#')
        || text.contains("](")
        || text.contains(" | ")
        || text.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with('>') || t.starts_with("1.") || t.starts_with('|')
        })
}

/// The destination URLs of every link in `text`, in document order — the same
/// order their labels appear in the rendered output, so the draw layer can zip
/// them onto the link cells it finds and overlay OSC 8 hyperlinks.
pub fn link_urls(text: &str) -> Vec<String> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    Parser::new_ext(text, opts)
        .filter_map(|ev| match ev {
            Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
            _ => None,
        })
        .collect()
}

/// Render `text` as Markdown into styled lines, using `base` as the body style.
pub fn render(text: &str, base: Style) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let mut r = Renderer::new(base);
    for ev in Parser::new_ext(text, opts) {
        r.event(ev);
    }
    r.finish()
}

struct Renderer {
    base: Style,
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    /// Inline-emphasis stack; the top is the active style.
    styles: Vec<Style>,
    /// List nesting: `None` = bullet, `Some(n)` = next ordered number.
    lists: Vec<Option<u64>>,
    quote: usize,
    /// Active fenced code block: the language token (may be empty).
    code: Option<String>,
    code_buf: String,
    /// A pending one-shot prefix (a list marker) for the next flushed line.
    pending_marker: Option<String>,
    /// Whether a blank separator is owed before the next block.
    spaced: bool,
    /// Active table being buffered (column widths need every row first).
    table: Option<TableState>,
}

/// A Markdown table buffered until `End(Table)`, when column widths are known.
struct TableState {
    aligns: Vec<Alignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    /// The row currently being filled (header or body).
    cur: Vec<String>,
    in_head: bool,
}

impl Renderer {
    fn new(base: Style) -> Self {
        Self {
            base,
            lines: Vec::new(),
            cur: Vec::new(),
            styles: vec![base],
            lists: Vec::new(),
            quote: 0,
            code: None,
            code_buf: String::new(),
            pending_marker: None,
            spaced: false,
            table: None,
        }
    }

    fn style(&self) -> Style {
        *self.styles.last().unwrap_or(&self.base)
    }

    /// Continuation indent for the current list nesting (2 spaces per level).
    fn indent(&self) -> String {
        " ".repeat(self.lists.len() * 2)
    }

    /// Quote prefix spans for the current blockquote depth.
    fn quote_prefix(&self) -> Vec<Span<'static>> {
        (0..self.quote)
            .map(|_| Span::styled("▏ ", Style::default().fg(RULE_FG)))
            .collect()
    }

    /// Push a blank line between blocks (deduped, never leading).
    fn blank(&mut self) {
        if !self.lines.is_empty() && !matches!(self.lines.last(), Some(l) if l.spans.is_empty()) {
            self.lines.push(Line::from(""));
        }
    }

    /// Emit the in-progress line (with quote/list prefixes) and start a fresh one.
    fn flush(&mut self) {
        if self.cur.is_empty() && self.pending_marker.is_none() {
            return;
        }
        let mut spans = self.quote_prefix();
        if let Some(marker) = self.pending_marker.take() {
            // Bullets/numbers sit at the parent indent; their text follows.
            let pad = " ".repeat(self.lists.len().saturating_sub(1) * 2);
            spans.push(Span::raw(pad));
            spans.push(Span::styled(
                marker,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if !self.lists.is_empty() && self.quote == 0 {
            spans.push(Span::raw(self.indent()));
        }
        spans.append(&mut self.cur);
        self.lines.push(Line::from(spans));
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if self.code.is_some() {
                    self.code_buf.push_str(&t);
                } else if let Some(tbl) = &mut self.table {
                    if let Some(cell) = tbl.cur.last_mut() {
                        cell.push_str(&t);
                    }
                } else {
                    let style = self.style();
                    self.cur.push(Span::styled(t.into_string(), style));
                }
            }
            Event::Code(t) => {
                if let Some(tbl) = &mut self.table {
                    if let Some(cell) = tbl.cur.last_mut() {
                        cell.push_str(&t);
                    }
                } else {
                    // Inline code.
                    self.cur.push(Span::styled(
                        format!(" {t} "),
                        Style::default().fg(INLINE_CODE_FG).bg(CODE_BG),
                    ));
                }
            }
            Event::SoftBreak => {
                // Soft wrap in source → a space; the paragraph re-wraps to width.
                let style = self.style();
                self.cur.push(Span::styled(" ", style));
            }
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.blank();
                self.lines.push(Line::from(Span::styled(
                    "──────────",
                    Style::default().fg(RULE_FG),
                )));
                self.spaced = true;
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                let style = self.style();
                self.cur.push(Span::styled(mark.to_string(), style));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph if self.spaced => self.blank(),
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.blank();
                let (hashes, fg) = match level {
                    HeadingLevel::H1 => ("# ", HEADING_FG),
                    HeadingLevel::H2 => ("## ", HEADING_FG),
                    _ => ("### ", Color::Rgb(205, 214, 244)),
                };
                self.cur.push(Span::styled(
                    hashes.to_string(),
                    Style::default().fg(RULE_FG),
                ));
                self.styles.push(
                    Style::default()
                        .fg(fg)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                );
            }
            Tag::BlockQuote(_) => {
                self.blank();
                self.quote += 1;
                self.styles
                    .push(self.style().add_modifier(Modifier::ITALIC).fg(Color::Gray));
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.blank();
                self.code = Some(match kind {
                    CodeBlockKind::Fenced(lang) => lang.into_string(),
                    CodeBlockKind::Indented => String::new(),
                });
                self.code_buf.clear();
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.pending_marker = Some(marker);
            }
            Tag::Emphasis => self
                .styles
                .push(self.style().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(self.style().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(self.style().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => {
                // Render the label in the link color + underline; the draw layer
                // detects these cells and overlays a real OSC 8 hyperlink (the
                // URL itself stays out of the visible text). See `link_urls`.
                self.styles
                    .push(self.style().fg(LINK_FG).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Table(aligns) => {
                self.flush();
                self.blank();
                self.table = Some(TableState {
                    aligns,
                    header: Vec::new(),
                    rows: Vec::new(),
                    cur: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.cur.clear();
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.cur.clear();
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.cur.push(String::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.spaced = true;
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
                self.spaced = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.styles.pop();
                self.quote = self.quote.saturating_sub(1);
                self.spaced = true;
            }
            TagEnd::CodeBlock => {
                let lang = self.code.take().unwrap_or_default();
                let code = std::mem::take(&mut self.code_buf);
                for l in highlight(&code, &lang) {
                    self.lines.push(l);
                }
                self.spaced = true;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.spaced = true;
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.header = std::mem::take(&mut t.cur);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    for l in render_table(&t, self.base) {
                        self.lines.push(l);
                    }
                    self.spaced = true;
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        // Trim a trailing blank line.
        while matches!(self.lines.last(), Some(l) if l.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

/// Render a buffered Markdown table into aligned, column-padded lines: a bold
/// header, a `─┼─` rule, then body rows separated by ` │ `. Column widths use
/// display width so wide glyphs (CJK / emoji like ✅) line up.
fn render_table(t: &TableState, base: Style) -> Vec<Line<'static>> {
    let ncols = t
        .header
        .len()
        .max(t.rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if ncols == 0 {
        return Vec::new();
    }
    fn cell(row: &[String], c: usize) -> &str {
        row.get(c).map(|s| s.trim()).unwrap_or("")
    }
    let mut w = vec![0usize; ncols];
    for (c, width) in w.iter_mut().enumerate() {
        *width = UnicodeWidthStr::width(cell(&t.header, c));
        for r in &t.rows {
            *width = (*width).max(UnicodeWidthStr::width(cell(r, c)));
        }
    }
    let align = |c: usize| t.aligns.get(c).copied().unwrap_or(Alignment::None);
    let pad = |s: &str, width: usize, a: Alignment| -> String {
        let fill = width.saturating_sub(UnicodeWidthStr::width(s));
        match a {
            Alignment::Right => format!("{}{s}", " ".repeat(fill)),
            Alignment::Center => {
                let l = fill / 2;
                format!("{}{s}{}", " ".repeat(l), " ".repeat(fill - l))
            }
            _ => format!("{s}{}", " ".repeat(fill)),
        }
    };
    let bar = || Span::styled(" │ ", Style::default().fg(RULE_FG));
    let mut out = Vec::new();

    let header_style = Style::default().fg(HEADING_FG).add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::raw("  ")];
    for (c, width) in w.iter().enumerate() {
        if c > 0 {
            spans.push(bar());
        }
        spans.push(Span::styled(
            pad(cell(&t.header, c), *width, align(c)),
            header_style,
        ));
    }
    out.push(Line::from(spans));

    let mut sep = String::from("  ");
    for (c, width) in w.iter().enumerate() {
        if c > 0 {
            sep.push_str("─┼─");
        }
        sep.push_str(&"─".repeat(*width));
    }
    out.push(Line::from(Span::styled(sep, Style::default().fg(RULE_FG))));

    for r in &t.rows {
        let mut spans = vec![Span::raw("  ")];
        for (c, width) in w.iter().enumerate() {
            if c > 0 {
                spans.push(bar());
            }
            spans.push(Span::styled(pad(cell(r, c), *width, align(c)), base));
        }
        out.push(Line::from(spans));
    }
    out
}

/// Syntax-highlight a fenced code block into indented, colored lines with a
/// subtle background so it reads as a block. Falls back to plain text for
/// unknown languages.
fn highlight(code: &str, lang: &str) -> Vec<Line<'static>> {
    let ps = syntaxes();
    let syntax = (!lang.is_empty())
        .then(|| ps.find_syntax_by_token(lang))
        .flatten()
        .or_else(|| ps.find_syntax_by_first_line(code.lines().next().unwrap_or("")))
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let mut spans = vec![Span::styled("  ", Style::default().bg(CODE_BG))];
        match h.highlight_line(line, ps) {
            Ok(ranges) => {
                for (sty, text) in ranges {
                    let t = text.trim_end_matches('\n');
                    if t.is_empty() {
                        continue;
                    }
                    spans.push(Span::styled(t.to_string(), syn_style(sty)));
                }
            }
            Err(_) => spans.push(Span::styled(
                line.trim_end_matches('\n').to_string(),
                Style::default().bg(CODE_BG),
            )),
        }
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled("  ", Style::default().bg(CODE_BG))));
    }
    out
}

/// Convert a syntect style to a ratatui style (RGB fg + emphasis + code bg).
fn syn_style(s: syntect::highlighting::Style) -> Style {
    let fg = Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b);
    let mut style = Style::default().fg(fg).bg(CODE_BG);
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn headings_lists_and_emphasis() {
        let base = Style::default();
        let md = "# Title\n\nSome **bold** and *italic* text.\n\n- one\n- two\n";
        let lines = render(md, base);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.contains("Title")));
        assert!(t.iter().any(|l| l.contains("bold")));
        assert!(t.iter().any(|l| l.contains("• one")));
        assert!(t.iter().any(|l| l.contains("• two")));
    }

    #[test]
    fn fenced_code_is_highlighted() {
        let md = "before\n\n```rust\nfn main() { let x = 1; }\n```\n\nafter";
        let lines = render(md, Style::default());
        // The code line carries multiple colored spans (highlighting kicked in).
        let code_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("fn")))
            .expect("code line present");
        assert!(
            code_line.spans.len() > 1,
            "code is split into colored spans"
        );
        // The fence markers themselves are not rendered.
        assert!(!texts(&lines).iter().any(|l| l.contains("```")));
    }

    #[test]
    fn ordered_list_numbers() {
        let lines = render("1. first\n2. second\n", Style::default());
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.contains("1. first")));
        assert!(t.iter().any(|l| l.contains("2. second")));
    }

    #[test]
    fn table_columns_align() {
        let md = "| Format | Fast |\n|---|---|\n| Markdown | yes |\n| HTML | no |\n";
        let lines = render(md, Style::default());
        let t = texts(&lines);
        // Header, separator (┼), and both body rows are present and the cells
        // are not concatenated into one run.
        assert!(t.iter().any(|l| l.contains("Format") && l.contains("Fast")));
        assert!(t.iter().any(|l| l.contains('┼')));
        assert!(t
            .iter()
            .any(|l| l.contains("Markdown") && l.contains("yes")));
        assert!(t.iter().any(|l| l.contains("HTML") && l.contains("no")));
        // A column separator is rendered between cells.
        assert!(t.iter().any(|l| l.contains('│')));
    }

    #[test]
    fn link_label_renders_and_url_is_collected() {
        // The label renders (styled), the URL stays out of the visible text, and
        // `link_urls` recovers the URL in order for the OSC 8 overlay.
        let lines = render("see [the spec](https://example.com/spec)", Style::default());
        let joined: String = texts(&lines).join("\n");
        assert!(joined.contains("the spec"));
        assert!(!joined.contains("https://example.com/spec"));
        assert_eq!(
            link_urls("see [the spec](https://example.com/spec) and [b](http://b.io)"),
            vec!["https://example.com/spec", "http://b.io"]
        );
    }

    #[test]
    fn plain_prose_detection() {
        assert!(!looks_like_markdown("just a plain sentence with no markup"));
        assert!(looks_like_markdown("has **bold**"));
        assert!(looks_like_markdown("```code```"));
    }
}
