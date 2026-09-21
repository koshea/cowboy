//! `cowboy init` — create initial project config files under `.cowboy/`.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use cowboy_core::config::{self, ConfigPaths, ProvidersConfig};

use crate::cli::InitArgs;
use crate::style;

pub fn run(args: InitArgs) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);

    fs::create_dir_all(&paths.dir).with_context(|| format!("creating {}", paths.dir.display()))?;

    // One confirmation for the whole scaffold rather than one per file: `--force`
    // overwrites `security.yaml`, which is the file the boundary is built from, so
    // clobbering hand-edited mounts and grants should never be a silent side effect of
    // re-running init.
    if args.force {
        let existing: Vec<&Path> = [paths.security.as_path(), paths.agent.as_path()]
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if !existing.is_empty() {
            crate::ui::warn("--force will overwrite existing config:");
            for p in &existing {
                crate::ui::kv("overwrite", &p.display().to_string());
            }
            if !crate::prompt::confirm_destructive("Overwrite?")? {
                return Ok(());
            }
        }
    }

    write_file(&paths.security, &config::security_template(), args.force)?;
    write_file(&paths.agent, &config::agent_template(), args.force)?;
    // Note: models/providers are NOT scaffolded into the project. Provider
    // credentials are host-owned (home dir); see `cowboy models setup`.

    ensure_gitignore(&root)?;

    if args.git {
        maybe_git_init(&root)?;
    }

    // Offer to approve Compose networks for the agent (interactive only).

    println!(
        "\n{} {}",
        style::success("Initialized cowboy config in"),
        paths.dir.display()
    );
    println!("  - {} (host-owned, never mounted)", config::SECURITY_FILE);
    println!("  - {} (read by the agent loop)", config::AGENT_FILE);

    // Point the user at provider setup if no home provider is configured yet.
    let has_provider = ProvidersConfig::load_global()
        .map(|p| !p.providers.is_empty())
        .unwrap_or(false);
    if has_provider {
        println!("\n{} run `cowboy doctor`.", style::bold("Next:"));
    } else {
        println!(
            "\n{} run `cowboy models setup` to configure a model provider, then `cowboy doctor`.",
            style::bold("Next:")
        );
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        println!(
            "  {} {} (exists; use --force to overwrite)",
            style::dim("skip  "),
            path.display()
        );
        return Ok(());
    }
    let verb = if path.exists() { "rewrote" } else { "created" };
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    println!("  {} {}", style::green(verb), path.display());
    Ok(())
}

/// Ensure `.gitignore` ignores secrets and session artifacts.
fn ensure_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    // Ranch plans + their published artifacts are committed (shared source of
    // truth); only per-ranch runtime/scratch files are ignored.
    let wanted = [
        ".env",
        ".cowboy/sessions/",
        ".cowboy/mise/",
        ".cowboy/ranches/*/events.jsonl",
        ".cowboy/ranches/*/workstreams/",
        "/target",
    ];
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut additions = String::new();
    for entry in wanted {
        if !existing.lines().any(|l| l.trim() == entry) {
            additions.push_str(entry);
            additions.push('\n');
        }
    }
    if additions.is_empty() {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n# cowboy\n");
    out.push_str(&additions);
    fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    println!("  updated {}", path.display());
    Ok(())
}

fn maybe_git_init(root: &Path) -> Result<()> {
    if root.join(".git").exists() {
        return Ok(());
    }
    let status = Command::new("git")
        .arg("init")
        .current_dir(root)
        .status()
        .context("running `git init` (is git installed?)")?;
    if status.success() {
        println!("  ran    git init");
    } else {
        anyhow::bail!("git init failed with status {status}");
    }
    Ok(())
}
