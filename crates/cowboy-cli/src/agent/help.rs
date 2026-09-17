//! The one place that knows what keys and slash commands exist.
//!
//! There used to be three lists, and they disagreed. `HELP_LINES` was a 25-line block
//! dumped into the transcript; `COMMAND_COMPLETIONS` was a separate table used by the
//! autocomplete popup; and the actual behaviour was a `match` in `handle_command`. The
//! popup omitted `/jobs`, `/after` and `/queue` entirely — commands that existed, worked,
//! and were invisible unless you had read the help. Any of the three could be updated
//! without the others.
//!
//! So: one table, three consumers (the help overlay, the completion catalog, and a guard
//! test in `tests/help_surface.rs` that asserts the table and the dispatcher name the same
//! commands). Adding a slash command means adding a row here, or the guard fails.

use cowboy_tui::{Completion, HelpSection};

/// Which part of the help overlay a row belongs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Steering a run: interrupt, queue, redirect.
    Run,
    /// Looking at what happened: diff, context, copy.
    Inspect,
    /// Delegated work: subagents and jobs.
    Crew,
    /// Configuration: model, crew roster, MCP.
    Setup,
    /// Starting and stopping: detach, quit.
    Session,
}

impl Group {
    /// Section heading, and the order sections appear in.
    const ORDER: [Group; 5] = [
        Group::Run,
        Group::Crew,
        Group::Inspect,
        Group::Setup,
        Group::Session,
    ];

    fn title(self) -> &'static str {
        match self {
            Group::Run => "Steering a run",
            Group::Crew => "Delegated work",
            Group::Inspect => "Looking at what happened",
            Group::Setup => "Setup",
            Group::Session => "Session",
        }
    }
}

/// One slash command.
pub struct Slash {
    /// The name typed after `/`.
    pub name: &'static str,
    /// Argument shape shown after the name, or `""`.
    pub args: &'static str,
    pub help: &'static str,
    pub group: Group,
    /// Extra spellings accepted by the dispatcher. Listed so the guard test can tell an
    /// alias from a command that is missing from this table.
    pub aliases: &'static [&'static str],
    /// Only offered inside a ranch workstream.
    pub workstream_only: bool,
}

/// Every slash command, in the order each group shows them.
pub const SLASH: &[Slash] = &[
    Slash {
        name: "plan",
        args: "<task>",
        help: "research first — file edits are blocked until you approve",
        group: Group::Run,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "go",
        args: "[note]",
        help: "approve the plan and let the agent start editing",
        group: Group::Run,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "after",
        args: "<msg>",
        help: "queue a message for after this turn instead of steering it",
        group: Group::Run,
        aliases: &["then"],
        workstream_only: false,
    },
    Slash {
        name: "queue",
        args: "[clear]",
        help: "show the queued messages, or drop them",
        group: Group::Run,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "skills",
        args: "",
        help: "list the skills this project defines",
        group: Group::Run,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "jobs",
        args: "",
        help: "the background subagents and their turn usage",
        group: Group::Crew,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "ranch",
        args: "[note]",
        help: "promote this discussion into a multi-workstream ranch plan",
        group: Group::Crew,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "accept",
        args: "[note]",
        help: "sign off this ranch workstream and advance the plan",
        group: Group::Crew,
        aliases: &[],
        workstream_only: true,
    },
    Slash {
        name: "diff",
        args: "",
        help: "the working-tree diff",
        group: Group::Inspect,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "context",
        args: "",
        help: "context-window usage and what is filling it",
        group: Group::Inspect,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "copy",
        args: "",
        help: "copy the last answer to the system clipboard",
        group: Group::Inspect,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "clear",
        args: "",
        help: "clear the view (the conversation is kept)",
        group: Group::Inspect,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "model",
        args: "[name]",
        help: "show or switch the active model",
        group: Group::Setup,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "models",
        args: "",
        help: "browse the provider catalogue and add a model",
        group: Group::Setup,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "crew",
        args: "[usage]",
        help: "the crew roster (which model each role gets), or its usage",
        group: Group::Setup,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "mcp",
        args: "",
        help: "the connected MCP servers (manage them with `cowboy mcp`)",
        group: Group::Setup,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "help",
        args: "[topic]",
        help: "this reference, optionally filtered",
        group: Group::Session,
        aliases: &["h", "?"],
        workstream_only: false,
    },
    Slash {
        name: "detach",
        args: "",
        help: "leave the session running and exit (re-attach later)",
        group: Group::Session,
        aliases: &[],
        workstream_only: false,
    },
    Slash {
        name: "quit",
        args: "",
        help: "end the session",
        group: Group::Session,
        aliases: &["exit", "q"],
        workstream_only: false,
    },
];

/// One key binding.
pub struct Hotkey {
    pub keys: &'static str,
    pub help: &'static str,
    pub group: Group,
}

/// Every key binding.
///
/// Only modified keys and function keys appear here, and that is a constraint rather than
/// a preference: in the editing modes an unmodified letter is *typed text*, so a bare `j`
/// could never be a global hotkey. The `Alt-` set was chosen from the combinations
/// `ratatui-textarea` does not already bind, with one deliberate exception noted below.
pub const KEYS: &[Hotkey] = &[
    Hotkey {
        keys: "Enter",
        help: "send · Shift/Alt+Enter for a newline",
        group: Group::Run,
    },
    Hotkey {
        // The headline change: this used to open a menu you then picked a letter from,
        // which is a strange thing to put between a user and a runaway command.
        keys: "Ctrl-C",
        help: "interrupt the turn · clears the input if idle · twice ends the session",
        group: Group::Run,
    },
    Hotkey {
        keys: "type while running",
        help: "steers the turn in flight (use /after to queue instead)",
        group: Group::Run,
    },
    Hotkey {
        keys: "↑ / ↓",
        help: "previous / next message you sent",
        group: Group::Run,
    },
    Hotkey {
        keys: "Alt-j",
        help: "what the subagents are doing",
        group: Group::Crew,
    },
    Hotkey {
        keys: "Alt-w",
        help: "watch a subagent's live output (again to cycle, Esc to leave)",
        group: Group::Crew,
    },
    Hotkey {
        keys: "Alt-s",
        help: "stop the subagents, leaving this turn running",
        group: Group::Crew,
    },
    Hotkey {
        keys: "PgUp / PgDn",
        help: "scroll · Shift+↑↓ by a line · Shift+End back to the tail",
        group: Group::Inspect,
    },
    Hotkey {
        keys: "drag, then y",
        help: "select and copy (Esc clears); /copy takes the whole last answer",
        group: Group::Inspect,
    },
    Hotkey {
        keys: "Ctrl-L",
        help: "redraw, if something corrupts the screen",
        group: Group::Inspect,
    },
    Hotkey {
        keys: "F1",
        help: "this reference",
        group: Group::Session,
    },
    Hotkey {
        // Takes Alt-d from the textarea's delete-next-word. Alt-Delete still does that,
        // and detaching earns a key more than an emacs alias does.
        keys: "Alt-d",
        help: "detach — leave it running in the background and exit",
        group: Group::Session,
    },
];

/// What the caller knows about this session, for the context-dependent rows.
pub struct Ctx<'a> {
    pub workstream: bool,
    /// Skill names discovered in the project, with a usage hint each.
    pub skills: &'a [(String, String)],
}

/// Is `name` a slash command we know (by name or alias)?
pub fn is_known(name: &str) -> bool {
    SLASH
        .iter()
        .any(|s| s.name == name || s.aliases.contains(&name))
}

/// The command `typed` most likely meant, if any is close enough.
///
/// Edit distance capped at 2, and never suggesting something less than half the length of
/// what was typed: `/x` is not a typo for `/quit`, and offering it would be worse than
/// saying nothing.
pub fn nearest(typed: &str) -> Option<&'static str> {
    let budget = if typed.len() <= 3 { 1 } else { 2 };
    SLASH
        .iter()
        .map(|s| (edit_distance(typed, s.name), s.name))
        .filter(|(d, _)| *d <= budget)
        .min_by_key(|(d, name)| (*d, name.len()))
        .map(|(_, name)| name)
}

/// Levenshtein distance, single-row DP.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The help overlay's sections, optionally filtered to rows matching `query`.
///
/// A filter matches the key/command *or* its description, so `/help subagent` finds the
/// crew keys without the reader having to know they are called that.
pub fn sections(ctx: &Ctx, query: Option<&str>) -> Vec<HelpSection> {
    let q = query.map(str::to_lowercase);
    let keep = |a: &str, b: &str| match &q {
        None => true,
        Some(q) => a.to_lowercase().contains(q) || b.to_lowercase().contains(q),
    };

    let mut out = Vec::new();
    for g in Group::ORDER {
        let mut rows: Vec<(String, String)> = Vec::new();
        for k in KEYS.iter().filter(|k| k.group == g) {
            if keep(k.keys, k.help) {
                rows.push((k.keys.to_string(), k.help.to_string()));
            }
        }
        for s in SLASH.iter().filter(|s| s.group == g) {
            if s.workstream_only && !ctx.workstream {
                continue;
            }
            let label = if s.args.is_empty() {
                format!("/{}", s.name)
            } else {
                format!("/{} {}", s.name, s.args)
            };
            if keep(&label, s.help) {
                rows.push((label, s.help.to_string()));
            }
        }
        if !rows.is_empty() {
            out.push(HelpSection {
                title: g.title().to_string(),
                rows,
            });
        }
    }

    // Project skills are `/name` commands too, but they are discovered rather than
    // declared, so they get their own section instead of being mixed into `Run`.
    if !ctx.skills.is_empty() {
        let rows: Vec<(String, String)> = ctx
            .skills
            .iter()
            .filter(|(n, h)| keep(n, h))
            .map(|(n, h)| (format!("/{n}"), h.clone()))
            .collect();
        if !rows.is_empty() {
            out.push(HelpSection {
                title: "Skills in this project".to_string(),
                rows,
            });
        }
    }
    out
}

/// The autocomplete catalog: every command available here, plus the project's skills.
pub fn completions(ctx: &Ctx) -> Vec<Completion> {
    let mut out: Vec<Completion> = SLASH
        .iter()
        .filter(|s| !s.workstream_only || ctx.workstream)
        .map(|s| Completion {
            value: s.name.to_string(),
            hint: if s.args.is_empty() {
                s.help.to_string()
            } else {
                format!("{} {}", s.args, s.help)
            },
        })
        .collect();
    out.extend(ctx.skills.iter().map(|(n, h)| Completion {
        value: n.clone(),
        hint: h.clone(),
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx<'static> {
        Ctx {
            workstream: false,
            skills: &[],
        }
    }

    #[test]
    fn every_command_lands_in_a_rendered_section() {
        // A row with a `Group` no section renders would vanish from the help without any
        // compiler complaint, since `Group::ORDER` is a separate list from the enum.
        let sections = sections(&ctx(), None);
        let rendered: usize = sections.iter().map(|s| s.rows.len()).sum();
        let expected = KEYS.len() + SLASH.iter().filter(|s| !s.workstream_only).count();
        assert_eq!(
            rendered,
            expected,
            "{} row(s) belong to a group Group::ORDER does not list",
            expected - rendered
        );
    }

    #[test]
    fn a_workstream_only_command_is_hidden_outside_one() {
        let outside = sections(&ctx(), None);
        assert!(!flat(&outside).iter().any(|(k, _)| k == "/accept [note]"));
        let inside = sections(
            &Ctx {
                workstream: true,
                skills: &[],
            },
            None,
        );
        assert!(flat(&inside).iter().any(|(k, _)| k == "/accept [note]"));
    }

    #[test]
    fn a_filter_matches_the_description_not_only_the_name() {
        // The point of filtering: you look up what you want to do, not what it is called.
        let s = sections(&ctx(), Some("subagent"));
        let rows = flat(&s);
        assert!(!rows.is_empty(), "expected matches for 'subagent'");
        assert!(rows.iter().any(|(k, _)| k == "Alt-s"));
        // And nothing unrelated came along.
        assert!(!rows.iter().any(|(k, _)| k == "/diff"));
    }

    #[test]
    fn an_unmatched_filter_yields_no_sections_rather_than_empty_ones() {
        assert!(sections(&ctx(), Some("zzz-nope")).is_empty());
    }

    #[test]
    fn aliases_are_known_but_are_not_offered_as_completions() {
        assert!(is_known("quit") && is_known("q") && is_known("exit"));
        assert!(!is_known("nonesuch"));
        let values: Vec<String> = completions(&ctx()).into_iter().map(|c| c.value).collect();
        assert!(values.contains(&"quit".to_string()));
        // Offering `/q` and `/exit` beside `/quit` triples the popup for no new ability.
        assert!(!values.contains(&"q".to_string()));
    }

    #[test]
    fn commands_that_take_arguments_say_so_in_the_completion_hint() {
        let c = completions(&ctx());
        let after = c.iter().find(|c| c.value == "after").unwrap();
        assert!(after.hint.starts_with("<msg>"), "got {:?}", after.hint);
    }

    #[test]
    fn a_near_miss_gets_a_suggestion_and_a_wild_guess_does_not() {
        assert_eq!(nearest("diff"), Some("diff"));
        assert_eq!(nearest("dif"), Some("diff"));
        assert_eq!(nearest("contxt"), Some("context"));
        assert_eq!(nearest("detatch"), Some("detach"));
        // Too short to be a typo of anything: suggesting `/quit` for `/x` is worse than
        // saying nothing, because it looks like the CLI knows something you don't.
        assert_eq!(nearest("x"), None);
        assert_eq!(nearest("frobnicate"), None);
    }

    #[test]
    fn an_ambiguous_typo_resolves_to_the_shorter_command() {
        // `moldes` is two edits from both `model` and `models`. Prefer the shorter, which
        // is also the one that does less: `/model` reports, `/models` opens a picker, and
        // a wrong guess that only prints is the cheaper wrong guess.
        assert_eq!(nearest("moldes"), Some("model"));
    }

    fn flat(s: &[HelpSection]) -> Vec<(String, String)> {
        s.iter().flat_map(|s| s.rows.clone()).collect()
    }
}
