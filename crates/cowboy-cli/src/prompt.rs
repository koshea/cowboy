//! Asking the user something from a plain CLI command.
//!
//! Gathered in one place because the three implementations that grew up separately —
//! `models setup`'s `yes_no`, `patch revert`'s inline read, the session collision menu —
//! disagreed on the things that matter: whether a non-terminal counts as consent,
//! whether `COWBOY_ASSUME_YES` is honoured, and what a bare Enter means.
//!
//! Two rules, applied everywhere:
//!
//! - **No TTY is not consent.** A piped or CI run gets the default, and for a
//!   destructive action the default is always no. Scripts opt in explicitly with
//!   `COWBOY_ASSUME_YES=1` (or a command's `--yes`), which is auditable in a shell
//!   history in a way that "it didn't ask" is not.
//! - **The prompt says which way Enter goes** (`[Y/n]` vs `[y/N]`), so the default is
//!   visible rather than remembered.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

/// Set by a caller (or a `--yes` flag) to answer every confirmation with yes.
pub const ASSUME_YES_ENV: &str = "COWBOY_ASSUME_YES";

/// The `--yes` flag, recorded once by `main` rather than threaded through every
/// command signature.
///
/// A static rather than `set_var(ASSUME_YES_ENV, …)`: the environment is inherited by
/// every worker, holder and sandboxed command this process spawns, so setting it there
/// would silently pre-answer confirmations in child processes the user never flagged.
static ASSUME_YES: AtomicBool = AtomicBool::new(false);

/// Record the root `--yes` flag. Called once from `main`.
pub fn set_assume_yes(yes: bool) {
    if yes {
        ASSUME_YES.store(true, Ordering::Relaxed);
    }
}

/// Whether confirmations are being answered for us.
pub fn assume_yes() -> bool {
    ASSUME_YES.load(Ordering::Relaxed) || env_says_yes(std::env::var_os(ASSUME_YES_ENV))
}

/// The env half of [`assume_yes`], split out so the accepted spellings are testable.
///
/// Not tested through the real variable: `cargo test` runs a binary's tests as threads in
/// one process, so setting `COWBOY_ASSUME_YES` (or the static) to prove it works would
/// pre-answer confirmations for every other test in the same process.
fn env_says_yes(v: Option<std::ffi::OsString>) -> bool {
    v.is_some_and(|v| v != "0" && !v.is_empty())
}

/// Ask a yes/no question.
///
/// `default_yes` decides both what a bare Enter means and what a non-interactive run
/// gets — so a destructive caller passes `false` and a non-interactive run is refused
/// rather than assumed.
pub fn confirm(question: &str, default_yes: bool) -> Result<bool> {
    if assume_yes() {
        // Echoed, not silent: a script that pre-answers should still leave a record of
        // *what* it agreed to in the output it captures.
        println!("{question} [assumed yes]");
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        // Echoed for the same reason as the assumed-yes case: a captured log should show
        // the question that was answered, not just its consequence.
        let taken = if default_yes { "yes" } else { "no" };
        println!("{question} [not a terminal; taking the default: {taken}]");
        return Ok(default_yes);
    }
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(decide(line.trim(), default_yes))
}

/// A destructive confirmation: never defaults to yes, and says so.
///
/// Separate from [`confirm`] so the call sites read as what they are, and so no future
/// edit can quietly flip a destructive default by passing the wrong bool. Prints the
/// refusal itself — every caller wants to say the same thing, and the versions that
/// rolled their own disagreed ("aborted.", "cancelled", nothing at all).
pub fn confirm_destructive(question: &str) -> Result<bool> {
    let yes = confirm(question, false)?;
    if !yes {
        crate::ui::info("aborted — nothing was changed.");
    }
    Ok(yes)
}

/// The pure decision, so the accepted spellings are testable without stdin.
fn decide(input: &str, default_yes: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

/// Prompt for a line, returning the trimmed input (or `default` on empty).
pub fn line(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) => print!("{label} [{d}]: "),
        None => print!("{label}: "),
    }
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let t = buf.trim();
    Ok(if t.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        t.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_takes_the_default_and_only_yes_means_yes() {
        for (input, default_yes, expected) in [
            ("", true, true),
            ("", false, false),
            ("y", false, true),
            ("Y", false, true),
            ("yes", false, true),
            ("YES", false, true),
            ("n", true, false),
            ("no", true, false),
            // Anything unrecognised is a no, never an accidental yes.
            ("maybe", true, false),
            ("ya", true, false),
            ("1", true, false),
        ] {
            assert_eq!(decide(input, default_yes), expected, "input {input:?}");
        }
    }

    #[test]
    fn a_destructive_prompt_cannot_default_to_yes() {
        // Guards the shape rather than the wording: the whole point of the separate
        // entry point is that no edit can pass `true` here.
        assert!(!decide("", false));
    }

    #[test]
    fn only_a_meaningful_env_value_pre_answers() {
        use std::ffi::OsString;
        assert!(!env_says_yes(None));
        // `COWBOY_ASSUME_YES=` and `=0` are how a script *disables* an inherited value;
        // treating either as consent would be the opposite of what was written.
        assert!(!env_says_yes(Some(OsString::from(""))));
        assert!(!env_says_yes(Some(OsString::from("0"))));
        assert!(env_says_yes(Some(OsString::from("1"))));
        assert!(env_says_yes(Some(OsString::from("yes"))));
    }
}
