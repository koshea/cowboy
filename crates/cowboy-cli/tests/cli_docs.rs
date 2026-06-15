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

use clap::CommandFactory;
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
    let mut subs: Vec<_> = cmd
        .get_subcommands()
        .filter(|s| s.get_name() != "help")
        .collect();
    subs.sort_by_key(|s| s.get_name().to_string());
    for s in subs {
        render_command(s, &format!("{full} {}", s.get_name()), depth + 1, out);
    }
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
