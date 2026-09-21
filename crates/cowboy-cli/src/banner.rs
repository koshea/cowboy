//! The session-start banner: a short branded intro animated at the top of the TUI
//! transcript, in place of the old "Welcome to cowboy" line.
//!
//! This module owns the **art only**. It renders frames as ANSI-coloured strings and
//! hands them to `App::begin_intro`, which pushes them into the transcript as
//! `LineKind::Art` lines and rewrites them in place as it ticks. Two consequences
//! worth knowing:
//!
//! - The intro lives in the transcript, so it scrolls away with the rest of the
//!   welcome block rather than pinning a header no one needs after the first turn.
//! - Animating it invalidates the transcript's memoized line cache on every frame.
//!   That is affordable *only* because this runs at session start, when the
//!   transcript is the welcome block and nothing else. It would not be affordable
//!   later, which is why nothing else animates through the transcript.
//!
//! The accent colour is derived from the project path, so every repo gets a stable
//! identity and a glance at a window tells you which one you are in.

use std::path::Path;

/// Wordmark rows. Every row is exactly [`ART_W`] columns, which the wipe and the
/// frame-height arithmetic both rely on — `wordmark_rows_are_uniform` pins it.
const WORDMARK: [&str; 3] = [
    "╔═╗╔═╗╦ ╦╔╗ ╔═╗╦ ╦",
    "║  ║ ║║║║╠╩╗║ ║╚╦╝",
    "╚═╝╚═╝╚╩╝╚═╝╚═╝ ╩ ",
];
/// Width of the wordmark in columns.
const ART_W: usize = 18;
/// Left margin for the whole scene.
const INDENT: &str = "  ";
/// Below this many usable columns the scene doesn't fit, so we skip it rather than
/// wrap it into nonsense. The scene is as wide as [`TAGLINE`] plus both margins.
const MIN_COLS: u16 = TAGLINE.len() as u16 + 6;
/// Widest the horizon will grow. Matched to the tagline so the horizon rule and
/// the text beneath it square off into one block instead of one overhanging the
/// other. `tagline_is_ascii` pins the `len()`-as-columns assumption.
const MAX_HORIZON: usize = TAGLINE.len();

/// `horizon_w` clamps into `ART_W..=MAX_HORIZON`, and `clamp` panics on an inverted
/// range — so a tagline shortened below the wordmark's width would be a crash rather
/// than a layout wobble. Caught at compile time instead.
const _: () = assert!(MAX_HORIZON >= ART_W);

/// Milliseconds per frame. ~22 fps: smooth enough for a wipe, and the whole intro is
/// over in a bit more than a second. The event loop raises its poll rate to match
/// while the intro is running, and drops back to its idle cadence afterwards.
pub const FRAME_MS: u64 = 45;
/// The tagline typed in during the settle phase. It replaces the line the welcome
/// block used to open with, so the text is not said twice.
const TAGLINE: &str = "the agent runs in a sandbox built from your machine";

/// Tumbleweed glyph cycle — it tumbles as it rolls.
const WEED: [char; 4] = ['◍', '◌', '◎', '○'];
/// Per-project accent palette (xterm-256 indices): amber, cactus, dusty rose,
/// sky, sand, sage. Chosen to stay legible on both light and dark backgrounds.
const ACCENTS: [u8; 6] = [214, 71, 174, 110, 180, 108];
/// Fixed colours: the hot edge of the branding iron, the cactus, and the dim
/// furniture (sky, fence, horizon, tagline).
const HOT: u8 = 231;
const CACTUS: u8 = 71;
const DIM: u8 = 240;

/// How the banner should be shown, from `COWBOY_BANNER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// Play the animation, then settle.
    Animated,
    /// Show the settled frame only — no motion.
    Static,
    /// Show nothing.
    Off,
}

/// Read `COWBOY_BANNER`. Presence-based booleans are the convention elsewhere, but
/// this one has three meaningful states, so it takes a value like
/// `COWBOY_ASSUME_YES` does. Unset means animated.
fn style() -> Style {
    style_from(std::env::var("COWBOY_BANNER").ok().as_deref())
}

/// The parse half of [`style`], split out so it is testable without mutating the
/// process environment (which is global, and so races every other test).
fn style_from(value: Option<&str>) -> Style {
    let Some(v) = value else {
        return Style::Animated;
    };
    match v.trim().to_ascii_lowercase().as_str() {
        // An empty value turns an inherited setting back off, as `COWBOY_ASSUME_YES`
        // does — otherwise `COWBOY_BANNER=` would read as "on".
        "" | "0" | "off" | "false" | "no" => Style::Off,
        "static" | "plain" | "1" => Style::Static,
        _ => Style::Animated,
    }
}

/// Rows every frame occupies: the sky, the three wordmark rows, the ground, the
/// horizon rule, and the tagline. `a_frame_is_art_rows_tall` pins it.
const ART_ROWS: usize = 7;
/// Rows the welcome block needs below the art (workspace/model/session/skills, the
/// openers, the prompt hint). Below `ART_ROWS + this` the art would simply scroll
/// out of view as the welcome text pushed it up, so there is no point drawing it.
const WELCOME_ROWS: usize = 8;

/// The intro's frames, ready for `App::begin_intro`, or empty when there should be
/// no art at all.
///
/// `cols` and `rows` describe the space available *inside the transcript pane*, not
/// the terminal: the transcript is a fraction of the screen, and the art must both
/// fit its width without wrapping and have room to stay on screen underneath the
/// welcome lines that follow it.
///
/// Every frame has the same number of rows and no row contains a newline, both of
/// which the in-place rewrite depends on.
pub fn intro_frames(root: &Path, cols: u16, rows: u16) -> Vec<Vec<String>> {
    // `NO_COLOR` means no decoration; without colour the branding wipe is just
    // flicker, so the art is skipped rather than shown grey.
    if cols < MIN_COLS
        || (rows as usize) < ART_ROWS + WELCOME_ROWS
        || std::env::var_os("NO_COLOR").is_some()
    {
        return Vec::new();
    }
    let accent = accent_for(root);
    let horizon = horizon_w(cols);
    match style() {
        Style::Off => Vec::new(),
        Style::Static => vec![settled(horizon, accent)],
        Style::Animated => {
            let mut frames = frames(horizon, accent);
            // The settled frame is last so that skipping — which jumps to the final
            // frame — always lands on a finished wordmark.
            frames.push(settled(horizon, accent));
            frames
        }
    }
}

/// The full frame sequence: brand the wordmark, roll a tumbleweed across the
/// horizon, then type the tagline in.
fn frames(horizon: usize, accent: u8) -> Vec<Vec<String>> {
    let mut frames = Vec::new();
    // Phase 1 — branding iron: two columns per frame, hottest at the leading edge.
    let mut revealed = 0;
    while revealed < ART_W {
        revealed = (revealed + 2).min(ART_W);
        frames.push(scene(horizon, accent, revealed, None, 0));
    }
    // Phase 2 — the tumbleweed crosses.
    const STOPS: usize = 12;
    for i in 0..STOPS {
        let x = i * horizon / STOPS;
        frames.push(scene(
            horizon,
            accent,
            ART_W,
            Some((x, WEED[i % WEED.len()])),
            0,
        ));
    }
    // Phase 3 — settle: the tagline types in.
    let tag_len = TAGLINE.chars().count();
    let mut shown = 0;
    while shown < tag_len {
        shown = (shown + 8).min(tag_len);
        frames.push(scene(horizon, accent, ART_W, None, shown));
    }
    frames
}

/// The resting frame: full wordmark, quiet horizon, whole tagline.
fn settled(horizon: usize, accent: u8) -> Vec<String> {
    scene(horizon, accent, ART_W, None, TAGLINE.chars().count())
}

/// Build one frame. Row count is fixed for every frame, and **each element is
/// exactly one row** — the in-place rewrite replaces a fixed slice of transcript
/// lines, so an element containing a newline would desynchronise the art from the
/// lines it occupies. `no_frame_element_spans_two_rows` pins it.
fn scene(
    horizon: usize,
    accent: u8,
    revealed: usize,
    weed: Option<(usize, char)>,
    tagline: usize,
) -> Vec<String> {
    let mut lines = Vec::with_capacity(7);
    lines.push(format!("{INDENT}{}", fg(DIM, "·      ⌐¬")));
    for row in WORDMARK {
        lines.push(format!("{INDENT}{}", brand(row, revealed, accent)));
    }
    lines.push(format!("{INDENT}{}", ground(horizon, accent, weed)));
    lines.push(format!("{INDENT}{}", fg(DIM, &"━".repeat(horizon))));
    lines.push(format!(
        "{INDENT}{}",
        fg(DIM, &tagline_text(tagline.min(horizon)))
    ));
    lines
}

/// The wordmark revealed to `n` columns, with the two cells the iron is touching
/// left glowing.
fn brand(row: &str, n: usize, accent: u8) -> String {
    let chars: Vec<char> = row.chars().collect();
    let n = n.min(chars.len());
    if n == chars.len() {
        return fg(accent, row);
    }
    let cut = n.saturating_sub(2);
    let cool: String = chars[..cut].iter().collect();
    let hot: String = chars[cut..n].iter().collect();
    format!("{}{}", fg(accent, &cool), fg(HOT, &hot))
}

/// The ground row — cactus, fence post, and the tumbleweed if it is on screen. The
/// horizon rule it sits on is a separate row (see [`scene`]).
fn ground(w: usize, accent: u8, weed: Option<(usize, char)>) -> String {
    // (glyph, colour); colour 0 means "no styling", i.e. a plain space.
    let mut cells: Vec<(char, u8)> = vec![(' ', 0); w];
    if w > 4 {
        cells[3] = ('Ψ', CACTUS);
    }
    if w > 9 {
        cells[8] = ('╷', DIM);
    }
    if let Some((x, glyph)) = weed {
        if x < w {
            cells[x] = (glyph, accent);
        }
    }
    let mut row = String::new();
    for (glyph, color) in cells {
        if color == 0 {
            row.push(glyph);
        } else {
            row.push_str(&fg(color, &glyph.to_string()));
        }
    }
    // Trailing spaces are invisible but wasteful; `draw` clears the line anyway.
    row.trim_end().to_string()
}

/// The tagline truncated to `n` characters.
fn tagline_text(n: usize) -> String {
    TAGLINE.chars().take(n).collect()
}

/// Wrap `text` in an xterm-256 foreground colour. Indexed rather than truecolour:
/// 256-colour support is close to universal, 24-bit is not.
fn fg(color: u8, text: &str) -> String {
    format!("\x1b[38;5;{color}m{text}\x1b[0m")
}

/// The project's accent colour, stable for a given path.
///
/// Keyed on the *worktree* path via [`crate::project::project_hash`], so two
/// worktrees of one repo read as different windows — which is the case where
/// telling them apart matters most.
fn accent_for(root: &Path) -> u8 {
    let h = crate::project::project_hash(root);
    ACCENTS[(h % ACCENTS.len() as u64) as usize]
}

/// Horizon width: as wide as the terminal allows, capped so it stays a horizon
/// rather than a rule across a very wide window.
fn horizon_w(cols: u16) -> usize {
    let usable = cols.saturating_sub(INDENT.len() as u16 * 2) as usize;
    usable.clamp(ART_W, MAX_HORIZON)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wipe slices every row at the same column and `draw` walks back a fixed
    /// number of rows, so a row of the wrong width would tear the frame.
    #[test]
    fn wordmark_rows_are_uniform() {
        for row in WORDMARK {
            assert_eq!(
                row.chars().count(),
                ART_W,
                "row {row:?} is not {ART_W} wide"
            );
        }
    }

    /// Every frame must have the same row count: the animation rewrites a fixed slice
    /// of transcript lines in place, so a frame of a different height could not be
    /// written into it. `App::begin_intro` refuses a ragged set for the same reason.
    #[test]
    fn every_frame_has_the_same_height() {
        let frames = frames(MAX_HORIZON, ACCENTS[0]);
        let settled = settled(MAX_HORIZON, ACCENTS[0]);
        let h = settled.len();
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.len(), h, "frame {i} has {} rows, expected {h}", f.len());
        }
    }

    /// One element, one row.
    ///
    /// The animation replaces transcript line `at + i` with frame row `i`, so an
    /// element containing a newline would put two screen rows into one transcript
    /// line and desynchronise the art from the space reserved for it. An earlier
    /// version of this did exactly that — `ground()` returned the objects row *and*
    /// the horizon rule joined by a newline — and it looked fine in a screenshot of
    /// the final frame, which is why this asserts on the frame data instead.
    #[test]
    fn no_frame_element_spans_two_rows() {
        let mut all = frames(MAX_HORIZON, ACCENTS[0]);
        all.push(settled(MAX_HORIZON, ACCENTS[0]));
        for (i, frame) in all.iter().enumerate() {
            for (j, line) in frame.iter().enumerate() {
                assert!(
                    !line.contains('\n') && !line.contains('\r'),
                    "frame {i} row {j} spans multiple rows: {line:?}"
                );
            }
        }
    }

    /// A pane too small for the art gets no art, rather than a wrapped mess or a
    /// wordmark that scrolls out of sight the moment the welcome text lands.
    #[test]
    fn a_pane_too_small_gets_no_intro() {
        let root = Path::new("/srv/proj");
        let tall = (ART_ROWS + WELCOME_ROWS) as u16;
        assert!(
            intro_frames(root, MIN_COLS - 1, tall).is_empty(),
            "too narrow"
        );
        assert!(
            intro_frames(root, MIN_COLS, tall - 1).is_empty(),
            "too short"
        );
        assert!(!intro_frames(root, MIN_COLS, tall).is_empty(), "should fit");
    }

    /// `ART_ROWS` is what the height gate is computed from, so it has to match what
    /// `scene` actually builds.
    #[test]
    fn a_frame_is_art_rows_tall() {
        assert_eq!(settled(MAX_HORIZON, ACCENTS[0]).len(), ART_ROWS);
    }

    /// End-to-end: the art this module generates, pushed through the real `App` and
    /// the real `draw`, lands in the transcript unwrapped and animates.
    ///
    /// The unit tests above check the frame *data*; this checks the thing that
    /// actually matters — that a row of the wordmark survives the transcript's own
    /// wrapping at a realistic pane width, and that ticking advances what is drawn.
    /// The two crates meet only here, so this is the only place it can be checked.
    #[test]
    fn the_art_renders_unwrapped_in_the_real_transcript_and_advances() {
        use cowboy_tui::{draw, App, LineKind};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        // A 120-column terminal gives the transcript pane ~79 columns.
        let pane_w = 79u16;
        let frames = intro_frames(Path::new("/srv/proj"), pane_w, 30);
        assert!(!frames.is_empty(), "no frames at a realistic pane size");
        let rows = frames[0].len();

        let screen = |app: &App| {
            let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
            term.draw(|f| draw(f, app)).unwrap();
            let buf = term.backend().buffer().clone();
            let mut out = String::new();
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    out.push_str(buf[(x, y)].symbol());
                }
                out.push('\n');
            }
            out
        };

        let mut app = App::new("cowboy");
        app.begin_intro(frames.clone(), FRAME_MS, 0);
        app.push(LineKind::Banner, "workspace  /srv/proj");

        // The first frame is on screen, and the art occupies exactly `rows` lines —
        // if any row wrapped, the wordmark's rows would not be vertically adjacent.
        let first = screen(&app);
        assert!(
            first.contains(WORDMARK[0]) || first.contains(&WORDMARK[0][..6]),
            "no wordmark in the first frame:\n{first}"
        );
        assert_eq!(
            app.transcript
                .iter()
                .filter(|l| l.kind == LineKind::Art)
                .count(),
            rows,
            "art does not occupy one transcript line per frame row"
        );

        // Settled: the whole wordmark and the tagline are present and unwrapped.
        app.finish_intro();
        let last = screen(&app);
        for row in WORDMARK {
            assert!(
                last.contains(row.trim_end()),
                "wordmark row {row:?} wrapped or missing:\n{last}"
            );
        }
        assert!(
            last.contains(TAGLINE),
            "tagline wrapped or missing:\n{last}"
        );
        // And the welcome line after it is still there, below the art.
        assert!(last.contains("workspace  /srv/proj"));
    }

    /// The brand wipe must reach a fully revealed wordmark, and the settled frame
    /// must contain the whole tagline — a skipped animation lands on it directly.
    #[test]
    fn the_animation_resolves_to_a_complete_wordmark_and_tagline() {
        let settled = settled(MAX_HORIZON, ACCENTS[0]);
        let joined = settled.join("\n");
        for row in WORDMARK {
            assert!(
                joined.contains(row.trim_end()),
                "missing wordmark row {row:?}"
            );
        }
        assert!(joined.contains(TAGLINE), "settled frame lost the tagline");
        // Nothing should still be glowing once the iron has lifted.
        assert!(
            !joined.contains(&format!("\x1b[38;5;{HOT}m")),
            "settled frame still has a hot edge"
        );
    }

    /// `MIN_COLS` and `MAX_HORIZON` both treat `TAGLINE.len()` as a column count,
    /// which only holds while it is ASCII.
    #[test]
    fn tagline_is_ascii() {
        assert!(TAGLINE.is_ascii(), "TAGLINE must stay ASCII: {TAGLINE:?}");
        assert_eq!(TAGLINE.len(), TAGLINE.chars().count());
    }

    /// The accent is cosmetic but must not flicker between runs for one path.
    #[test]
    fn accent_is_stable_per_path_and_in_palette() {
        let a = accent_for(Path::new("/srv/proj"));
        let b = accent_for(Path::new("/srv/proj"));
        assert_eq!(a, b);
        assert!(ACCENTS.contains(&a));
    }

    /// `COWBOY_BANNER` is the documented off switch; the empty value turns it off
    /// like `COWBOY_ASSUME_YES` does, so an inherited value can be cleared.
    #[test]
    fn banner_style_reads_its_env_var() {
        assert_eq!(style_from(None), Style::Animated);
        for v in ["", "0", "off", "FALSE", " no "] {
            assert_eq!(style_from(Some(v)), Style::Off, "COWBOY_BANNER={v:?}");
        }
        for v in ["static", "plain", "1"] {
            assert_eq!(style_from(Some(v)), Style::Static, "COWBOY_BANNER={v:?}");
        }
        assert_eq!(style_from(Some("yes")), Style::Animated);
    }
}
