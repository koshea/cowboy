//! Terminal styling for **end-user** CLI output (`doctor`, `init`, `models`,
//! `down`, `sessions`, …).
//!
//! Color is emitted only when stdout is a TTY and `NO_COLOR` is unset, so piped
//! or redirected output stays plain. Agent/worker streaming output has its own
//! rendering (`agent/ui.rs`, the TUI) and does not use this.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Whether to emit ANSI styling (computed once; environment/TTY don't change
/// mid-run).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

const RESET: &str = "\x1b[0m";

/// Wrap `s` in `code` (and reset) when color is enabled, else return it plain.
fn paint(code: &str, s: &str) -> String {
    if enabled() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint("\x1b[1m", s)
}
pub fn dim(s: &str) -> String {
    paint("\x1b[2m", s)
}
pub fn green(s: &str) -> String {
    paint("\x1b[32m", s)
}
pub fn yellow(s: &str) -> String {
    paint("\x1b[33m", s)
}
pub fn red(s: &str) -> String {
    paint("\x1b[31m", s)
}
pub fn cyan(s: &str) -> String {
    paint("\x1b[36m", s)
}

/// Bold green — success/affirmative.
pub fn success(s: &str) -> String {
    paint("\x1b[1;32m", s)
}
/// Bold yellow — warning.
pub fn warning(s: &str) -> String {
    paint("\x1b[1;33m", s)
}
/// Bold red — error/failure.
pub fn error(s: &str) -> String {
    paint("\x1b[1;31m", s)
}
