//! Guards on how commands talk to the user.
//!
//! These are conventions, not logic, so nothing else can catch them: a reviewer has to
//! notice, and reviewers did not — five different success markers and a `Debug`-printed
//! enum in an interactive prompt all shipped. Both checks read the source of
//! `src/cmd/`, the same trick `cli_docs` uses to keep the generated CLI reference
//! honest.

use std::path::{Path, PathBuf};

fn cmd_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd")
}

/// Every `.rs` under `src/cmd`, as (path, source).
fn command_sources() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![cmd_dir()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src/cmd") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let src = std::fs::read_to_string(&path).expect("read source");
                out.push((path, src));
            }
        }
    }
    assert!(out.len() > 20, "expected the whole command surface");
    out
}

/// Scan the code of `src/cmd` line by line for `is_offender`, skipping any line whose
/// preceding lines carry an opt-out `marker`.
///
/// Markers go *above* the line, not on it: they explain why the exception exists, and an
/// explanation belongs on its own line.
fn scan(marker: &str, is_offender: impl Fn(&str) -> bool) -> Vec<String> {
    scan_with(marker, |lines, i| {
        Some(lines[i].trim_start().to_string()).filter(|t| is_offender(t))
    })
}

/// Like [`scan`], but the unit is a whole `print!`/`println!` *statement*.
///
/// rustfmt breaks any multi-argument print so the format string lands on the line after
/// the macro, so a check that only saw the macro line missed most of the codebase — and
/// did, until `artifact add`'s hand-rolled `✓` turned up.
fn scan_prints(marker: &str, is_offender: impl Fn(&str) -> bool) -> Vec<String> {
    scan_with(marker, |lines, i| {
        if !prints(lines[i].trim_start()) {
            return None;
        }
        let stmt = lines[i..]
            .iter()
            .take(24)
            .take_while_inclusive(|l| !l.trim_end().ends_with(");"))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        Some(stmt).filter(|s| is_offender(s))
    })
}

/// The shared walk: `unit(lines, i)` returns the offending text starting at line `i`, or
/// `None`. An opt-out `marker` anywhere in the three lines above — or inside the unit
/// itself, for a multi-line statement — exempts it.
fn scan_with(marker: &str, unit: impl Fn(&[&str], usize) -> Option<String>) -> Vec<String> {
    let mut offenders = Vec::new();
    for (path, src) in command_sources() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Everything from `mod tests` on is test scaffolding, which is allowed to be
        // scrappy about output.
        let body = match src.find("\nmod tests {") {
            Some(i) => &src[..i],
            None => src.as_str(),
        };
        let lines: Vec<&str> = body.lines().collect();
        for i in 0..lines.len() {
            let t = lines[i].trim_start();
            if t.starts_with("//") {
                continue;
            }
            let Some(text) = unit(&lines, i) else {
                continue;
            };
            let exempt = lines[..i]
                .iter()
                .rev()
                .take(3)
                .copied()
                .chain(text.lines())
                .any(|l| l.contains(marker));
            if !exempt {
                offenders.push(format!("{name}:{}: {t}", i + 1));
            }
        }
    }
    offenders
}

/// `Iterator::take_while` but keeping the element that ended it, so a statement includes
/// its own closing line.
trait TakeWhileInclusive: Iterator + Sized {
    fn take_while_inclusive<P: FnMut(&Self::Item) -> bool>(
        self,
        mut pred: P,
    ) -> impl Iterator<Item = Self::Item> {
        let mut done = false;
        self.take_while(move |item| {
            if done {
                return false;
            }
            if !pred(item) {
                done = true;
            }
            true
        })
    }
}

impl<I: Iterator> TakeWhileInclusive for I {}

fn prints(line: &str) -> bool {
    line.starts_with("println!(") || line.starts_with("print!(")
}

/// `{:?}` renders a Rust value, not a sentence. It reached users in the interactive
/// worktree-collision prompt as `AwaitingApproval`, which is how you can tell nobody
/// read the output.
///
/// Deliberate uses (Debug as a quoting mechanism in a paste-able snippet) opt out with a
/// `debug-ok:` comment above the line.
#[test]
fn user_facing_output_never_prints_debug() {
    let offenders = scan_prints("debug-ok:", |stmt| stmt.contains(":?}"));
    assert!(
        offenders.is_empty(),
        "these print a Rust value where a user expects words — give the type a `Display` \
         impl, or mark the line `debug-ok:` if `{{:?}}` is doing string quoting:\n{}",
        offenders.join("\n")
    );
}

/// Confirmations go through [`cowboy_cli::prompt`], so there is one answer to "is a
/// non-TTY consent?" (no) and one place that honours `--yes` / `COWBOY_ASSUME_YES`.
///
/// `patch revert` is why this exists: it grew its own `[y/N]` read and its own env check,
/// so it treated a piped stdin as "no input, therefore no" while `models setup` treated
/// the same situation as yes. A command that genuinely needs a different shape — the
/// worktree-collision *menu* is multiple-choice, not yes/no — opts out with `prompt-ok:`.
#[test]
fn confirmations_go_through_the_shared_prompt() {
    let offenders = scan("prompt-ok:", |line| {
        line.contains("[y/N]")
            || line.contains("[Y/n]")
            || line.contains("ASSUME_YES")
            || (line.contains("stdin()") && line.contains("read_line"))
    });
    assert!(
        offenders.is_empty(),
        "ask with `prompt::confirm` / `prompt::confirm_destructive` / `prompt::line` \
         rather than reading stdin directly, so `--yes` and non-interactive runs behave \
         the same everywhere (mark a genuine exception `prompt-ok:`):\n{}",
        offenders.join("\n")
    );
}

/// One success marker, defined in one place (`ui::ok`).
///
/// There were five: `style::success`, a bare uncoloured `✓`, both together, no marker at
/// all, and `doctor`'s `[ ok ]` tags. Which one you saw depended on which command you
/// ran. A status *glyph table* (a legend, a per-row marker) is a different thing and opts
/// out with `legend-ok:`.
#[test]
fn success_is_reported_through_the_shared_helper() {
    let offenders = scan_prints("legend-ok:", |stmt| stmt.contains('✓'));
    assert!(
        offenders.is_empty(),
        "print success with `ui::ok(...)` so every command marks it the same way \
         (mark a status legend `legend-ok:`):\n{}",
        offenders.join("\n")
    );
}
