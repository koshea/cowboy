//! Slash commands in the web composer.
//!
//! Session commands (`/go`, `/plan`, `/accept`, skills, `/diff`, …) are not
//! interpreted here: they go to the worker as [`ClientMsg::Command`] and are
//! expanded by the same code the TUI uses (`cowboy_cli::agent::commands`). Only
//! commands about *this view* are handled locally.
//!
//! [`HELP`] must list every command in the TUI's table (`cowboy_cli::agent::help::
//! SLASH`); a test in cowboy-cli reads this file and fails if one is missing, so a
//! new TUI command can't quietly skip the web.

use cowboy_proto::daemonproto::ClientMsg;

use crate::model::Model;

/// `(command, args, what it does)` — the `/help` listing.
pub const HELP: &[(&str, &str, &str)] = &[
    ("help", "", "this list (aliases /h, /?)"),
    (
        "plan",
        "<task>",
        "research read-only and propose a plan; edits wait for /go",
    ),
    ("go", "[note]", "approve the plan and implement it"),
    (
        "after",
        "<msg>",
        "queue a message for after the current turn (alias /then)",
    ),
    ("queue", "[clear]", "show the queued messages, or drop them"),
    ("jobs", "", "the background subagents and their state"),
    (
        "stop",
        "",
        "stop the background subagents, leaving the session running",
    ),
    (
        "model",
        "[name]",
        "show or switch the model (from the next turn)",
    ),
    ("models", "", "list the configured models"),
    (
        "budget",
        "[on|off]",
        "the per-message turn limit; off lets a long task run without asking",
    ),
    ("context", "", "what fills the context window"),
    ("diff", "", "the working-tree diff"),
    ("copy", "", "copy the last answer"),
    ("clear", "", "clear this view (the conversation is kept)"),
    ("fold", "", "collapse command output and diffs"),
    ("unfold", "", "expand them again"),
    ("skills", "", "list skills; run one as /<name> [args]"),
    ("mcp", "", "the MCP servers this session can use"),
    ("boundary", "", "what the sandbox exposes"),
    ("crew", "[usage]", "the crew roster, or its usage"),
    (
        "ranch",
        "[note]",
        "promote this discussion into a multi-workstream ranch plan",
    ),
    (
        "accept",
        "[note]",
        "sign off on this ranch workstream and end the session",
    ),
    (
        "detach",
        "",
        "leave the session running and go back to the list",
    ),
    ("quit", "", "end the session (aliases /exit, /q, /end)"),
];

/// What the view should do with a `/command`.
pub enum Outcome {
    /// Send this to the worker.
    Send(ClientMsg),
    /// Show this line locally.
    Notice(String),
    ClearView,
    Fold(bool),
    Copy,
    Back,
}

/// Interpret `input` (with its leading `/`).
pub fn interpret(input: &str, model: &Model) -> Outcome {
    let body = input.trim().trim_start_matches('/');
    let (cmd, rest) = match body.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (body, ""),
    };
    match cmd {
        "help" | "h" | "?" => Outcome::Notice(help_text()),
        "clear" => Outcome::ClearView,
        "fold" => Outcome::Fold(true),
        "unfold" => Outcome::Fold(false),
        "copy" => Outcome::Copy,
        "detach" => Outcome::Back,
        "queue" if rest.is_empty() => Outcome::Notice(queue_text(model)),
        "jobs" => Outcome::Notice(jobs_text(model)),
        "context" => Outcome::Notice(context_text(model)),
        // The worker lists them with the current one marked.
        "models" => Outcome::Send(ClientMsg::Command("model".into())),
        _ => Outcome::Send(ClientMsg::Command(body.to_string())),
    }
}

fn help_text() -> String {
    let mut s = String::from("commands:");
    for (name, args, what) in HELP {
        let usage = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        s.push_str(&format!("\n  {usage:<18} {what}"));
    }
    s
}

fn queue_text(m: &Model) -> String {
    if m.queued.is_empty() {
        return "nothing queued — while the agent works, a message steers the current \
                turn; use /after <msg> to queue instead"
            .into();
    }
    let mut s = format!("{} queued message(s):", m.queued.len());
    for (i, q) in m.queued.iter().enumerate() {
        s.push_str(&format!("\n  {}. {q}", i + 1));
    }
    s.push_str("\n/queue clear drops them");
    s
}

fn jobs_text(m: &Model) -> String {
    if m.subagents.is_empty() {
        return "no background subagents".into();
    }
    let mut s = String::from("subagents:");
    for j in &m.subagents {
        let state = match j.done {
            Some(true) => "done",
            Some(false) => "failed",
            None if j.pending => "pending",
            None if j.asking => "asking a question",
            None if j.requested > 0 => "awaiting a turn grant",
            None => "running",
        };
        let turns = if j.granted > 0 {
            format!(" · {}/{} turns", j.used, j.granted)
        } else {
            String::new()
        };
        s.push_str(&format!(
            "\n  {} ({}) — {state} · {}s{turns}",
            j.label,
            j.model,
            j.elapsed_ms / 1000
        ));
        if !j.task.is_empty() {
            s.push_str(&format!("\n      {}", j.task));
        }
    }
    s
}

fn context_text(m: &Model) -> String {
    let Some(c) = &m.context else {
        return "no context usage reported yet (it arrives with the first request)".into();
    };
    let mut s = format!(
        "context: {} of {} budget ({}%) · window {} · reserve {}",
        c.used,
        c.budget,
        c.percent(),
        c.window,
        c.reserve
    );
    for (label, tokens) in &c.top {
        s.push_str(&format!("\n  {label:<22} {tokens}"));
    }
    s
}
