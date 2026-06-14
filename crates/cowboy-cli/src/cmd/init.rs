//! `cowboy init` — create initial project config files under `.cowboy/`.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use cowboy_core::config::{self, ConfigPaths};

use crate::cli::InitArgs;

pub fn run(args: InitArgs) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);

    fs::create_dir_all(&paths.dir).with_context(|| format!("creating {}", paths.dir.display()))?;

    write_file(&paths.security, &config::security_template(), args.force)?;
    write_file(&paths.agent, &config::agent_template(), args.force)?;
    write_file(&paths.models, &config::models_template(), args.force)?;

    ensure_gitignore(&root)?;

    if args.git {
        maybe_git_init(&root)?;
    }

    println!("\nInitialized cowboy config in {}", paths.dir.display());
    println!("  - {} (host-owned, never mounted)", config::SECURITY_FILE);
    println!("  - {} (mounted into the container)", config::AGENT_FILE);
    println!("  - {} (model profiles)", config::MODELS_FILE);
    println!("\nNext: set COWBOY_OPENAI_API_KEY, then run `cowboy doctor`.");
    Ok(())
}

fn write_file(path: &Path, contents: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        println!(
            "  skip   {} (exists; use --force to overwrite)",
            path.display()
        );
        return Ok(());
    }
    let verb = if path.exists() { "rewrote" } else { "created" };
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    println!("  {verb} {}", path.display());
    Ok(())
}

/// Ensure `.gitignore` ignores secrets and session artifacts.
fn ensure_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let wanted = [".env", ".cowboy/sessions/", "/target"];
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
