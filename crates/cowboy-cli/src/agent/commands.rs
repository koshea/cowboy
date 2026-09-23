//! Session slash commands, expanded in **one** place for every client.
//!
//! A `/go` typed into the TUI and one typed into the web composer must do the same
//! thing. They used not to: the TUI built its canned prompts and control messages
//! locally, and the web sent the raw text to the model — so `/go` from a phone left
//! plan mode on and every edit refused. Now a client sends
//! [`ClientMsg::Command`] and the worker expands it here into the ordinary
//! control messages, so a new client gets every command for free.
//!
//! Only *session* commands live here — ones that change what the worker does.
//! View commands (`/clear`, `/fold`, `/help`, `/copy`, …) stay in each client. The
//! command names and help text live in [`crate::agent::help::SLASH`].

use std::path::Path;

use cowboy_core::daemonproto::ClientMsg;

/// What a session command turned into.
#[derive(Debug, Clone, PartialEq)]
pub enum Expansion {
    /// Control messages to apply, in order, as if the client had sent them.
    Send(Vec<ClientMsg>),
    /// Nothing to do but tell the user something (usage, a listing, an error).
    Notice(String),
}

/// What the expansion needs to know about the session.
pub struct CommandCtx<'a> {
    pub root: &'a Path,
    /// Set for a ranch workstream session, the only place `/accept` means anything.
    pub workstream: bool,
    /// Configured model names, for `/model` with no argument.
    pub models: &'a [String],
    pub current_model: Option<&'a str>,
    /// The iteration budget as `(enforced, turns per message)`; `None` if unknown.
    pub budget: Option<(bool, u32)>,
}

/// Expand `/input` (without the leading slash). `None` means it isn't a session
/// command, so the client may handle it locally or report it unknown.
pub fn expand(input: &str, ctx: &CommandCtx) -> Option<Expansion> {
    let input = input.trim();
    let (cmd, rest) = match input.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (input, ""),
    };
    let send = |msgs: Vec<ClientMsg>| Some(Expansion::Send(msgs));
    let notice = |s: String| Some(Expansion::Notice(s));
    match cmd {
        "plan" if rest.is_empty() => notice(
            "usage: /plan <task> — the agent proposes a plan first; file edits stay \
             blocked until you approve with /go"
                .into(),
        ),
        "plan" => send(vec![
            ClientMsg::PlanMode(true),
            ClientMsg::Message(plan_prompt(rest)),
        ]),
        "go" => send(vec![
            ClientMsg::PlanMode(false),
            ClientMsg::Message(go_prompt(rest)),
        ]),
        "accept" if !ctx.workstream => {
            notice("/accept only applies to a ranch workstream session".into())
        }
        "accept" => send(vec![ClientMsg::Accept {
            note: (!rest.is_empty()).then(|| rest.to_string()),
        }]),
        "ranch" => send(vec![ClientMsg::Message(ranch_prompt(rest))]),
        "after" | "then" if rest.is_empty() => {
            notice("usage: /after <message> — queues it to run after the current turn".into())
        }
        "after" | "then" => send(vec![ClientMsg::Enqueue(rest.to_string())]),
        "queue" if rest == "clear" => send(vec![ClientMsg::QueueClear]),
        "model" if rest.is_empty() => notice(match ctx.current_model {
            Some(cur) if !ctx.models.is_empty() => {
                format!("model: {cur} (available: {})", ctx.models.join(", "))
            }
            Some(cur) => format!("model: {cur}"),
            None if !ctx.models.is_empty() => {
                format!("available models: {}", ctx.models.join(", "))
            }
            None => "usage: /model <name>".into(),
        }),
        "model" if !ctx.models.is_empty() && !ctx.models.iter().any(|m| m == rest) => {
            notice(format!(
                "unknown model {rest:?}; available: {}",
                ctx.models.join(", ")
            ))
        }
        "model" => send(vec![ClientMsg::SwitchModel(rest.to_string())]),
        "stop" => send(vec![ClientMsg::StopSubagents]),
        "budget" => match rest {
            "on" => send(vec![ClientMsg::IterationBudget(true)]),
            "off" => send(vec![ClientMsg::IterationBudget(false)]),
            "" => notice(match ctx.budget {
                Some((true, n)) => format!(
                    "iteration budget: on — the agent asks before going past {n} turns a \
                     message. /budget off to let long tasks run."
                ),
                Some((false, _)) => {
                    "iteration budget: off — long tasks run without asking. /budget on to \
                     restore."
                        .into()
                }
                None => "usage: /budget on|off".into(),
            }),
            _ => notice("usage: /budget [on|off]".into()),
        },
        "quit" | "exit" | "q" | "end" => send(vec![ClientMsg::End]),
        "skills" => notice(skills_listing(ctx.root)),
        "diff" => notice({
            let d = git_diff(ctx.root);
            if d.trim().is_empty() {
                "no working-tree changes".into()
            } else {
                d
            }
        }),
        "mcp" => notice(mcp_report(ctx.root).text()),
        "boundary" => notice(boundary_report(ctx.root).text()),
        "crew" => notice(crew_report((!rest.is_empty()).then_some(rest)).text()),
        other => cowboy_core::skills::load(ctx.root, other)
            .map(|skill| Expansion::Send(vec![ClientMsg::Message(skill_prompt(&skill, rest))])),
    }
}

/// The turn `/plan <task>` runs.
pub fn plan_prompt(task: &str) -> String {
    format!(
        "Plan mode is ON. Research the codebase READ-ONLY (read/grep/ls — do not \
         modify files or run state-changing commands), then present a concise, \
         numbered plan of the steps you'll take. Use the `plan` tool to list the \
         steps. Then stop and wait — I'll review and run /go to approve.\n\nTask: {task}"
    )
}

/// The turn `/go [note]` runs.
pub fn go_prompt(note: &str) -> String {
    let extra = if note.is_empty() {
        String::new()
    } else {
        format!(" Also: {note}")
    };
    format!("Approved — implement the plan now.{extra}")
}

/// The turn `/ranch [note]` runs.
pub fn ranch_prompt(note: &str) -> String {
    let extra = if note.is_empty() {
        String::new()
    } else {
        format!(" Emphasis: {note}.")
    };
    format!(
        "This is bigger than one session — promote it into a multi-workstream Ranch \
         Plan. Using what we've already discussed (don't re-research from scratch), \
         decompose the work into independent, parallelizable workstreams wired by \
         dependencies. Write the decomposition to `.cowboy/ranch-plan.yaml` with the \
         `write` tool (a YAML doc with `title`, `goal`, and a `workstreams` list — each \
         with `id`, `goal`, optional `title`, `depends_on`, `expected_artifacts`, \
         `acceptance`), then run `cowboy ranch draft .cowboy/ranch-plan.yaml` to \
         validate and draft it. Do not implement anything.{extra}"
    )
}

/// The turn `/<skill> [args]` runs: the skill's instructions, `$ARGUMENTS` filled.
pub fn skill_prompt(skill: &cowboy_core::skills::Skill, args: &str) -> String {
    let mut body = skill.instructions.clone();
    if body.contains("$ARGUMENTS") {
        body = body.replace("$ARGUMENTS", args);
    } else if !args.is_empty() {
        body.push_str(&format!("\n\nArguments: {args}"));
    }
    format!("Run the `{}` skill.\n\n{body}", skill.name)
}

fn skills_listing(root: &Path) -> String {
    let skills = cowboy_core::skills::discover(root);
    if skills.is_empty() {
        return "no skills found (.cowboy/skills or .claude/skills)".into();
    }
    let mut s = String::from("skills (run with `/<name> [args]`):");
    for skill in skills {
        let hint = skill
            .argument_hint
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "\n  /{}{hint}  — {}",
            skill.name, skill.description
        ));
    }
    s
}

/// Lines of a host-side report (`/diff`, `/mcp`, `/boundary`, `/crew`), built once
/// for every client: the TUI prints them, the worker sends them to a web client.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    /// `(is_error, line)`.
    pub lines: Vec<(bool, String)>,
}

impl Report {
    fn notice(&mut self, line: impl Into<String>) {
        self.lines.push((false, line.into()));
    }
    fn error(&mut self, line: impl Into<String>) {
        self.lines.push((true, line.into()));
    }
    /// The report as one block of text.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|(_, l)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `git diff` of the session's worktree.
pub fn git_diff(root: &Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("diff")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// `/mcp`: list the configured MCP servers (host + this repo's trust-gated
/// `.mcp.json`) as notices. Read-only — manage servers with the `cowboy mcp` CLI.
/// `/boundary`: what the sandbox actually allows — mounts, Landlock, seccomp, the
/// never-grantable paths, and the egress policy in force.
///
/// Read-only, and built from the same code path as `cowboy sandbox plan` so the two
/// cannot disagree. This is the one thing the UI was not showing about the product's
/// central claim: the status bar carried tokens, cost and diff, but nothing about
/// confinement.
pub fn boundary_report(root: &std::path::Path) -> Report {
    let mut r = Report::default();
    match crate::cmd::sandbox::describe(root) {
        Ok(report) => {
            r.notice(format!("boundary for {}", root.display()));
            for line in report.lines() {
                r.notice(line.to_string());
            }
        }
        Err(e) => r.error(format!("cannot describe the boundary: {e}")),
    }
    r
}

pub fn mcp_report(root: &std::path::Path) -> Report {
    let mut r = Report::default();
    let cfg = match cowboy_core::mcp::load_or_default() {
        Ok(cfg) => cfg,
        Err(e) => {
            r.error(format!("MCP config error: {e}"));
            return r;
        }
    };
    if cfg.servers.is_empty() {
        r.notice("no host MCP servers configured");
    } else {
        r.notice("MCP servers (host):");
        for (name, s) in &cfg.servers {
            let state = if s.enabled { "enabled" } else { "disabled" };
            let desc = if s.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", s.description)
            };
            r.notice(format!("  {name} [{state}] {}{desc}", s.transport_label()));
        }
    }
    // This repo's `.mcp.json`, if any (trust-gated).
    let state = crate::mcp::trust::project_trust(root);
    if state != crate::mcp::trust::TrustState::NoFile {
        if let Ok(Some(servers)) = cowboy_core::mcp::load_project_mcp(root) {
            r.notice(format!("MCP servers (.mcp.json) — {}:", state.label()));
            for (name, s) in &servers {
                r.notice(format!("  {name} {}", s.transport_label()));
            }
            if matches!(
                state,
                crate::mcp::trust::TrustState::Untrusted | crate::mcp::trust::TrustState::Stale
            ) {
                r.notice("  → enable with `cowboy mcp trust`");
            }
        }
    }
    r.notice("manage with `cowboy mcp add/trust/remove/test`");
    r
}

/// `/crew` (and `/crew usage`): show the crew roster matrix or usage summary as
/// notices. Read-only — manage the roster with the `cowboy crew` CLI.
pub fn crew_report(arg: Option<&str>) -> Report {
    let mut r = Report::default();
    use cowboy_core::crew;
    if arg == Some("usage") {
        let rows = crew::usage_by_model(&crew::load_history());
        if rows.is_empty() {
            r.notice("no recorded crew activity yet");
            return r;
        }
        r.notice("crew usage (per model):");
        for row in rows {
            r.notice(format!(
                "  {:<14} {} tasks · {}% ok · {}ms avg",
                row.model,
                row.tasks,
                row.success_pct(),
                row.avg_duration_ms()
            ));
        }
        return r;
    }
    match crew::load() {
        Ok(Some(cfg)) => {
            // Shorten ids to their last path segment so the grid stays readable.
            let short = |m: &str| m.rsplit('/').next().unwrap_or(m).to_string();
            let foreman =
                crate::cmd::crew::foreman_model().unwrap_or_else(|| "<default>".to_string());
            let mut col_w = crew::Effort::all()
                .iter()
                .map(|e| e.as_str().len())
                .max()
                .unwrap_or(6);
            for cat in cfg.crew.keys() {
                for (_, model) in cfg.expanded(cat, &foreman) {
                    col_w = col_w.max(short(&model).len());
                }
            }
            col_w += 2;
            r.notice(format!(
                "crew foreman: {}   delegation: {}",
                foreman,
                if cfg.enabled() { "on" } else { "off (solo)" }
            ));
            let mut header = format!("{:<14}", "CATEGORY");
            for e in crew::Effort::all() {
                header.push_str(&format!("{:<col_w$}", e.as_str()));
            }
            r.notice(header);
            for cat in cfg.crew.keys() {
                let mut row = format!("{cat:<14}");
                for (_, model) in cfg.expanded(cat, &foreman) {
                    row.push_str(&format!("{:<col_w$}", short(&model)));
                }
                r.notice(row);
            }
            r.notice("(edit with the `cowboy crew` CLI; `/crew usage` for activity)");
        }
        Ok(None) => r.notice("no crew roster — create one with `cowboy crew init`"),
        Err(e) => r.error(format!("crew: {e}")),
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(root: &Path) -> CommandCtx<'_> {
        CommandCtx {
            root,
            workstream: false,
            models: &[],
            current_model: None,
            budget: Some((true, 500)),
        }
    }

    #[test]
    fn go_leaves_plan_mode_then_runs_the_approval_turn() {
        let tmp = assert_fs::TempDir::new().unwrap();
        assert_eq!(
            expand("go ship it", &ctx(tmp.path())),
            Some(Expansion::Send(vec![
                ClientMsg::PlanMode(false),
                ClientMsg::Message("Approved — implement the plan now. Also: ship it".into()),
            ]))
        );
    }

    #[test]
    fn plan_needs_a_task_and_enters_plan_mode_first() {
        let tmp = assert_fs::TempDir::new().unwrap();
        assert!(matches!(
            expand("plan", &ctx(tmp.path())),
            Some(Expansion::Notice(_))
        ));
        let Some(Expansion::Send(msgs)) = expand("plan  add a cache ", &ctx(tmp.path())) else {
            panic!("expected messages");
        };
        assert_eq!(msgs[0], ClientMsg::PlanMode(true));
        assert!(matches!(&msgs[1], ClientMsg::Message(m) if m.ends_with("Task: add a cache")));
    }

    #[test]
    fn control_commands_map_to_their_messages() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let c = ctx(tmp.path());
        let one = |s| match expand(s, &c) {
            Some(Expansion::Send(mut v)) if v.len() == 1 => v.remove(0),
            other => panic!("{s}: {other:?}"),
        };
        assert_eq!(
            one("after run the tests"),
            ClientMsg::Enqueue("run the tests".into())
        );
        assert_eq!(one("then x"), ClientMsg::Enqueue("x".into()));
        assert_eq!(one("queue clear"), ClientMsg::QueueClear);
        assert_eq!(one("stop"), ClientMsg::StopSubagents);
        assert_eq!(one("quit"), ClientMsg::End);
        assert_eq!(one("model fast"), ClientMsg::SwitchModel("fast".into()));
        assert_eq!(one("budget off"), ClientMsg::IterationBudget(false));
        assert_eq!(one("budget on"), ClientMsg::IterationBudget(true));
        assert!(matches!(expand("budget", &c), Some(Expansion::Notice(n)) if n.contains("500")));
    }

    #[test]
    fn model_is_validated_against_known_names_when_there_are_any() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let models = ["fast".to_string(), "smart".to_string()];
        let c = CommandCtx {
            root: tmp.path(),
            workstream: false,
            models: &models,
            current_model: Some("fast"),
            budget: None,
        };
        assert!(
            matches!(expand("model nope", &c), Some(Expansion::Notice(n)) if n.contains("unknown"))
        );
        assert!(
            matches!(expand("model", &c), Some(Expansion::Notice(n)) if n.contains("model: fast"))
        );
        assert_eq!(
            expand("model smart", &c),
            Some(Expansion::Send(vec![ClientMsg::SwitchModel(
                "smart".into()
            )]))
        );
    }

    #[test]
    fn accept_is_refused_outside_a_workstream() {
        let tmp = assert_fs::TempDir::new().unwrap();
        assert!(matches!(
            expand("accept", &ctx(tmp.path())),
            Some(Expansion::Notice(_))
        ));
        let c = CommandCtx {
            workstream: true,
            ..ctx(tmp.path())
        };
        assert_eq!(
            expand("accept lgtm", &c),
            Some(Expansion::Send(vec![ClientMsg::Accept {
                note: Some("lgtm".into())
            }]))
        );
    }

    #[test]
    fn unknown_and_view_commands_are_left_to_the_client() {
        let tmp = assert_fs::TempDir::new().unwrap();
        assert_eq!(expand("clear", &ctx(tmp.path())), None);
        assert_eq!(expand("nonsense", &ctx(tmp.path())), None);
    }
}
