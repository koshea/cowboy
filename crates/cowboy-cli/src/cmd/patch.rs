//! `cowboy patch ...` — a stable CLI over git for making code changes visible
//! and reversible. Runs git on the host in the project root (the same files the
//! agent edits via the mounted workspace). Requires a git repository.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::cli::{PatchArgs, PatchCommand};

pub async fn run(args: PatchArgs) -> Result<()> {
    let root = crate::cmd::project_root()?;
    ensure_git_repo(&root)?;
    match args.command {
        PatchCommand::Show => show(&root),
        PatchCommand::Save => save(&root),
        PatchCommand::Apply => apply(&root, /* check_only */ false),
        PatchCommand::Check => apply(&root, /* check_only */ true),
        PatchCommand::Revert => revert(&root),
    }
}

/// Error clearly if the project is not a git repository.
fn ensure_git_repo(root: &Path) -> Result<()> {
    let ok = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!(
            "not a git repository: {}\nrun `cowboy init --git` or `git init` first",
            root.display()
        );
    }
    Ok(())
}

fn git_diff(root: &Path) -> Result<String> {
    let out = Command::new("git")
        .arg("diff")
        .current_dir(root)
        .output()
        .context("running git diff")?;
    if !out.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn show(root: &Path) -> Result<()> {
    let diff = git_diff(root)?;
    if diff.is_empty() {
        println!("(no uncommitted changes)");
    } else {
        print!("{diff}");
    }
    Ok(())
}

fn save(root: &Path) -> Result<()> {
    let diff = git_diff(root)?;
    let dir = root.join(".cowboy");
    std::fs::create_dir_all(&dir)?;
    let path: PathBuf = dir.join("diff.patch");
    std::fs::write(&path, &diff).with_context(|| format!("writing {}", path.display()))?;
    println!("saved {} ({} bytes)", path.display(), diff.len());
    Ok(())
}

fn apply(root: &Path, check_only: bool) -> Result<()> {
    let mut patch = String::new();
    std::io::stdin()
        .read_to_string(&mut patch)
        .context("reading patch from stdin")?;
    if patch.trim().is_empty() {
        bail!("no patch provided on stdin");
    }

    let mut cmd = Command::new("git");
    cmd.arg("apply");
    if check_only {
        cmd.arg("--check");
    }
    cmd.current_dir(root)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning git apply")?;
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .context("git apply stdin")?
        .write_all(patch.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "git apply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!(
        "{}",
        if check_only {
            "patch applies cleanly"
        } else {
            "patch applied"
        }
    );
    Ok(())
}

fn revert(root: &Path) -> Result<()> {
    let diff = git_diff(root)?;
    if diff.is_empty() {
        println!("(no uncommitted changes to revert)");
        return Ok(());
    }
    // `git checkout -- .` is unrecoverable: the changes were never committed, so there
    // is no reflog to walk back to. Show the scale before asking.
    let files = diff
        .lines()
        .filter(|l| l.starts_with("diff --git "))
        .count();
    crate::ui::warn(&format!(
        "reverting will discard uncommitted changes in {files} tracked file(s); this cannot be undone"
    ));
    crate::ui::step("`cowboy patch save` first if you want to keep them");
    if !crate::prompt::confirm_destructive("Revert ALL uncommitted changes?")? {
        return Ok(());
    }
    // Discard tracked changes; leave untracked files in place.
    let out = Command::new("git")
        .args(["checkout", "--", "."])
        .current_dir(root)
        .output()
        .context("running git checkout")?;
    if !out.status.success() {
        bail!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("reverted uncommitted tracked changes.");
    Ok(())
}
