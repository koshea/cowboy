//! Keeps the documentation in sync with the code.
//!
//! - `cli_reference_is_current` generates `docs/src/reference/cli.md` from the
//!   clap command tree and asserts the committed file matches, so the CLI
//!   reference can never silently drift. Regenerate after a CLI change with
//!   `COWBOY_REGEN_DOCS=1 cargo test -p cowboy-cli --test cli_docs`.
//! - `book_builds` runs `mdbook build docs` when `mdbook` is on PATH (skips
//!   otherwise), catching broken links / missing SUMMARY.md entries in CI.

use std::path::PathBuf;
use std::process::Command;

use clap::{CommandFactory, Parser};
use cowboy_cli::cli::Cli;

/// Workspace root (two levels up from this crate's manifest dir).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root")
}

fn cli_md_path() -> PathBuf {
    workspace_root().join("docs/src/reference/cli.md")
}

fn esc(s: &str) -> String {
    s.replace('\n', " ").replace('|', "\\|")
}

fn arg_label(a: &clap::Arg) -> String {
    if a.is_positional() {
        return a
            .get_value_names()
            .map(|vs| {
                vs.iter()
                    .map(|s| format!("<{s}>"))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_else(|| format!("<{}>", a.get_id()));
    }
    let mut s = String::new();
    if let Some(short) = a.get_short() {
        s.push_str(&format!("-{short}, "));
    }
    if let Some(long) = a.get_long() {
        s.push_str(&format!("--{long}"));
    }
    if s.is_empty() {
        s = a.get_id().to_string();
    }
    s
}

fn render_command(cmd: &clap::Command, full: &str, depth: usize, out: &mut String) {
    let hashes = "#".repeat((depth + 1).min(6));
    out.push_str(&format!("\n{hashes} `{full}`\n\n"));
    if let Some(about) = cmd.get_about() {
        out.push_str(&format!("{}\n\n", esc(&about.to_string())));
    }
    let args: Vec<_> = cmd
        .get_arguments()
        .filter(|a| a.get_id() != "help" && a.get_id() != "version")
        .collect();
    if !args.is_empty() {
        out.push_str("| Arg | Description |\n|-----|-------------|\n");
        for a in args {
            let help = a
                .get_help()
                .map(|h| esc(&h.to_string()))
                .unwrap_or_default();
            out.push_str(&format!("| `{}` | {} |\n", arg_label(a), help));
        }
        out.push('\n');
    }
    push_examples(cmd, out);
    let mut subs: Vec<_> = cmd
        .get_subcommands()
        .filter(|s| s.get_name() != "help")
        .collect();
    subs.sort_by_key(|s| s.get_name().to_string());
    for s in subs {
        render_command(s, &format!("{full} {}", s.get_name()), depth + 1, out);
    }
}

/// Emit a command's `after_help` block verbatim, in a fenced code block.
///
/// Verbatim because these blocks are already aligned two-column text: reflowing them into
/// markdown prose would lose the alignment that makes them scannable, and the fence keeps
/// mdbook from eating the `*` in `--tool '*'`.
fn push_examples(cmd: &clap::Command, out: &mut String) {
    let Some(after) = cmd.get_after_help() else {
        return;
    };
    out.push_str("```text\n");
    out.push_str(after.to_string().trim_end());
    out.push_str("\n```\n\n");
}

fn generate() -> String {
    let cmd = Cli::command();
    let mut out = String::new();
    out.push_str("# CLI reference\n\n");
    out.push_str(
        "<!-- GENERATED from the clap command tree by `cargo test -p cowboy-cli --test cli_docs`.\n\
         \x20    Do not edit by hand. Regenerate with:\n\
         \x20    COWBOY_REGEN_DOCS=1 cargo test -p cowboy-cli --test cli_docs -->\n\n",
    );
    if let Some(about) = cmd.get_about() {
        out.push_str(&format!("{}\n\n", esc(&about.to_string())));
    }
    // Root-level (global) options.
    let root_args: Vec<_> = cmd
        .get_arguments()
        .filter(|a| a.get_id() != "help" && a.get_id() != "version")
        .collect();
    if !root_args.is_empty() {
        out.push_str("## `cowboy` (global options)\n\n");
        out.push_str("| Arg | Description |\n|-----|-------------|\n");
        for a in root_args {
            let help = a
                .get_help()
                .map(|h| esc(&h.to_string()))
                .unwrap_or_default();
            out.push_str(&format!("| `{}` | {} |\n", arg_label(a), help));
        }
        out.push('\n');
    }
    push_examples(&cmd, &mut out);
    // Subcommands.
    let mut subs: Vec<_> = cmd
        .get_subcommands()
        .filter(|s| s.get_name() != "help")
        .collect();
    subs.sort_by_key(|s| s.get_name().to_string());
    for s in subs {
        render_command(s, &format!("cowboy {}", s.get_name()), 1, &mut out);
    }
    out
}

#[test]
fn cli_reference_is_current() {
    let generated = generate();
    let path = cli_md_path();
    if std::env::var("COWBOY_REGEN_DOCS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &generated).unwrap();
        eprintln!("regenerated {}", path.display());
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        committed, generated,
        "docs/src/reference/cli.md is stale. Regenerate with:\n  \
         COWBOY_REGEN_DOCS=1 cargo test -p cowboy-cli --test cli_docs"
    );
}

/// Every flag carries help text.
///
/// A flag with none renders as an empty cell in the generated reference and as a bare
/// `--session` in `--help`, which tells the reader nothing — nine of them shipped that
/// way, including three different `--session`s and `models add`'s `--temp`, `--context`
/// and `--max-output`. Positionals are exempt: their value name (`<RANCH>`,
/// `<WORKSTREAM>`) is usually the whole description, and forcing prose there produces
/// filler.
#[test]
fn every_flag_explains_itself() {
    fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<String>) {
        for a in cmd.get_arguments() {
            if a.is_positional() || a.get_id() == "help" || a.get_id() == "version" {
                continue;
            }
            if a.get_help().is_none() {
                out.push(format!("{path} {}", arg_label(a)));
            }
        }
        for s in cmd.get_subcommands() {
            if s.get_name() == "help" {
                continue;
            }
            walk(s, &format!("{path} {}", s.get_name()), out);
        }
    }
    let mut missing = Vec::new();
    walk(&Cli::command(), "cowboy", &mut missing);
    assert!(
        missing.is_empty(),
        "these flags have no help text, so `--help` and the CLI reference show a blank \
         description for them:\n{}",
        missing.join("\n")
    );
}

/// Every example in an `after_help` block is a real command.
///
/// The whole risk with worked examples is that they rot: a flag gets renamed, the example
/// keeps demonstrating the old spelling, and nothing fails. So each line beginning with
/// `cowboy ` is fed to the real parser. Consequences worth knowing when writing one: it
/// must be a complete command (no `…` placeholders), the explanation goes after a `#` so
/// the line is paste-able into a shell, and a redirection (`>`, `|`) is stripped rather
/// than parsed.
#[test]
fn every_example_in_help_actually_parses() {
    fn walk(cmd: &clap::Command, out: &mut Vec<(String, String)>) {
        let path = cmd.get_name().to_string();
        if let Some(after) = cmd.get_after_help() {
            for line in after.to_string().lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("cowboy") {
                    // Examples are written the way you would type them — trailing comment,
                    // redirection and all; only the argv part is the CLI's business.
                    let argv = rest.split(['#', '>', '|']).next().unwrap_or(rest).trim();
                    out.push((path.clone(), argv.to_string()));
                }
            }
        }
        for s in cmd.get_subcommands() {
            walk(s, out);
        }
    }
    let mut examples = Vec::new();
    walk(&Cli::command(), &mut examples);
    assert!(
        examples.len() > 20,
        "expected the examples to be found, got {}",
        examples.len()
    );

    let mut broken = Vec::new();
    for (owner, argv) in &examples {
        let Some(words) = shell_words(argv) else {
            broken.push(format!("{owner}: `cowboy {argv}` (unbalanced quotes)"));
            continue;
        };
        let full: Vec<String> = std::iter::once("cowboy".to_string()).chain(words).collect();
        if let Err(e) = Cli::try_parse_from(&full) {
            let first = e.to_string().lines().next().unwrap_or("").to_string();
            broken.push(format!("{owner}: `cowboy {argv}` -> {first}"));
        }
    }
    assert!(
        broken.is_empty(),
        "these `after_help` examples no longer parse — fix the example or the command:\n{}",
        broken.join("\n")
    );
}

/// Minimal POSIX-ish word split honouring single and double quotes. `None` if a quote is
/// left open, which is itself a broken example.
fn shell_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'') | (None, '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if started || !cur.is_empty() {
        words.push(cur);
    }
    Some(words)
}

#[test]
fn book_builds() {
    // Only when mdbook is available; skip cleanly otherwise (so local runs and CI
    // without the tool don't fail).
    if Command::new("mdbook").arg("--version").output().is_err() {
        eprintln!("skipping: mdbook not on PATH");
        return;
    }
    let docs = workspace_root().join("docs");
    let status = Command::new("mdbook")
        .arg("build")
        .arg(&docs)
        .status()
        .expect("run mdbook build");
    assert!(status.success(), "mdbook build failed");
}
