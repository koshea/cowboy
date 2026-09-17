//! One-shot hints, shown once per machine and then never again.
//!
//! There is one thing a new user reliably does not discover: **typing while the agent
//! works steers the turn in flight**. Every other affordance is reachable from `F1`, but
//! this one is invisible, because the natural assumption about a busy prompt is that it is
//! not listening. So it gets said once, at the moment it is true — the first time a turn
//! starts — and then stops.
//!
//! Once *per machine*, not per session: a hint that reappears every time you start work is
//! a hint you have learned to skip, which is worse than not showing it, because it also
//! trains you to skip the next one.
//!
//! Marks live host-side under `~/.config/cowboy/tips/`, alongside grants and approvals and
//! for the same reason as those: the workspace is writable from inside the sandbox, so a
//! marker there would let a repo suppress (or resurrect) the host's hints.

use std::path::PathBuf;

/// Typing during a turn steers it.
pub const STEERING: &str = "steering";

fn dir() -> Option<PathBuf> {
    Some(cowboy_core::config::global_config_dir()?.join("tips"))
}

/// Has `name` already been shown on this machine?
pub fn seen(name: &str) -> bool {
    // No home config dir (a bare container, say) counts as "seen": a hint is not worth
    // failing over, and showing it every single time is the outcome to avoid.
    dir().is_none_or(|d| d.join(name).exists())
}

/// Record `name` as shown. Best-effort: a hint we fail to remember is a hint shown twice,
/// which is not worth surfacing an error for.
pub fn mark(name: &str) {
    if let Some(d) = dir() {
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(d.join(name), b"");
    }
}

/// The text for `name` if it is due, marking it shown. `None` once it has been seen.
///
/// Combined rather than left to the caller so no call site can show a hint and forget to
/// mark it — which is the bug that turns a one-shot into a nag.
pub fn once(name: &str) -> Option<&'static str> {
    if seen(name) {
        return None;
    }
    mark(name);
    text(name)
}

fn text(name: &str) -> Option<&'static str> {
    match name {
        STEERING => Some(
            "tip: you can type while the agent works — your message reaches it at its \
             next step, so you do not have to wait or interrupt. /after <msg> queues one \
             for afterwards instead, and Ctrl-C stops the turn.",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tip_constant_has_text() {
        // The constant and the text are separate, so a new tip can be referenced from the
        // UI while `text` still returns None — showing nothing, silently. Written as a
        // slice so adding a second tip needs no restructuring.
        let all: &[&str] = &[STEERING];
        for name in all {
            assert!(text(name).is_some(), "no text for tip {name:?}");
        }
        assert_eq!(text("not-a-tip"), None);
    }

    #[test]
    fn a_tip_is_worth_reading() {
        // Guards against a placeholder: the whole justification for interrupting the
        // transcript is that the hint names the thing you could not have discovered.
        let t = text(STEERING).unwrap();
        assert!(t.contains("type while the agent works"));
        assert!(t.contains("/after"), "should point at the alternative");
    }
}
