//! Keeps the help overlay honest about what the TUI actually does.
//!
//! Three lists used to describe the slash commands — the help text, the autocomplete
//! catalog, and the `match` that implements them — and they disagreed: `/jobs`, `/after`
//! and `/queue` worked but appeared in neither of the other two, so the only way to find
//! them was to read the source. Nothing could fail, because nothing compared them.
//!
//! `agent/help.rs` is now the single table the overlay and the completions are built from.
//! What a table cannot enforce is that it matches the dispatcher, so this reads the arms
//! of `handle_command` out of the source (the same trick `cli_docs` uses) and compares
//! both directions.

use std::collections::BTreeSet;
use std::path::PathBuf;

use cowboy_cli::agent::help::{self, SLASH};

/// The command names the `match cmd { … }` in `handle_command` actually accepts.
fn dispatched() -> BTreeSet<String> {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "src/agent/tui.rs"]
        .iter()
        .collect();
    let src = std::fs::read_to_string(&path).expect("read tui.rs");
    let start = src.find("fn handle_command").expect("handle_command");
    let body = &src[start..];
    let arms = body.find("match cmd {").expect("the command match");

    let mut out = BTreeSet::new();
    // Arms sit at one indent inside `match cmd {`; string literals deeper than that are
    // message text in an arm's body, not patterns. Matching on indent rather than on
    // "starts with a quote" is the difference between reading the arm list and scraping
    // every literal in the function — the loose version collected sentences like
    // "cleared the view (conversation memory kept)" as command names.
    let indent = " ".repeat(8);
    for line in body[arms..].lines().skip(1) {
        let t = line.trim();
        // Stop at the catch-all: everything after it is the "unknown command" path (and
        // skill lookup), not a named command.
        if t.starts_with("other") {
            break;
        }
        if !line.starts_with(&indent) || line.starts_with(&format!("{indent} ")) {
            continue;
        }
        // An arm pattern is one or more string literals before `=>`.
        let Some(head) = t.split("=>").next() else {
            continue;
        };
        if !head.trim_start().starts_with('"') {
            continue;
        }
        for lit in head.split('|') {
            let lit = lit.trim().trim_matches('"');
            if !lit.is_empty() {
                out.insert(lit.to_string());
            }
        }
    }
    assert!(
        out.len() > 10,
        "the arm scan found only {out:?} — the match shape changed and this guard has \
         stopped guarding anything"
    );
    out
}

/// Every name the table declares, commands and aliases alike.
fn declared() -> BTreeSet<String> {
    SLASH
        .iter()
        .flat_map(|s| {
            std::iter::once(s.name.to_string()).chain(s.aliases.iter().map(|a| a.to_string()))
        })
        .collect()
}

#[test]
fn every_command_the_tui_dispatches_appears_in_help() {
    let (d, h) = (dispatched(), declared());
    let missing: Vec<&String> = d.difference(&h).collect();
    assert!(
        missing.is_empty(),
        "these slash commands work but are absent from `agent/help.rs`, so nothing tells \
         the user they exist: {missing:?}"
    );
}

#[test]
fn every_command_help_advertises_is_dispatched() {
    let (d, h) = (dispatched(), declared());
    let extra: Vec<&String> = h.difference(&d).collect();
    assert!(
        extra.is_empty(),
        "help advertises these but `handle_command` has no arm for them, so typing one \
         gets \"unknown command\": {extra:?}"
    );
}

#[test]
fn is_known_agrees_with_the_dispatcher() {
    // `is_known` is what decides whether an unrecognised `/x` is reported as a typo or
    // looked up as a skill, so it has to match the real arm list, aliases included.
    for name in dispatched() {
        assert!(help::is_known(&name), "is_known({name:?}) should be true");
    }
}
