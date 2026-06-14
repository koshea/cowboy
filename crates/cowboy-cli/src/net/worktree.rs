//! Git worktree helpers for running a session on its own branch/checkout.
//!
//! A new worktree gets a `cowboy/<slug>` branch and a sibling directory
//! `../<repo>-cowboy-<slug>`, suffixed on collision. Creating one never touches
//! the base branch's working tree (git checks out HEAD into the new path); we
//! only warn if the base is dirty. Worktrees and branches are never deleted by
//! cowboy.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use cowboy_core::daemonproto::{SessionStatus, WorktreeInfo};

/// A short branch slug from a task description (fallback `work`): lowercased,
/// non-alphanumerics collapsed to single dashes, trimmed, capped at 40 chars.
pub fn slugify(task: Option<&str>) -> String {
    let raw = task.unwrap_or("work").to_lowercase();
    let mut slug: String = raw
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    if slug.is_empty() {
        "work".into()
    } else {
        slug
    }
}

/// The top-level directory of the git repo containing `root`.
pub fn repo_root(root: &Path) -> Result<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running git rev-parse")?;
    if !out.status.success() {
        anyhow::bail!("{} is not inside a git repository", root.display());
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

/// Does the base repo have uncommitted changes? (Only used to warn — a new
/// worktree checks out committed HEAD, so dirty changes don't carry over.)
pub fn is_dirty(repo: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Choose a free `(branch, path)` for `slug`, adding a numeric suffix until both
/// the branch and the sibling directory are unused.
fn plan(repo: &Path, slug: &str) -> (String, PathBuf) {
    let parent = repo.parent().unwrap_or(repo);
    let name = repo
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    for n in 1.. {
        let suffixed = if n == 1 {
            slug.to_string()
        } else {
            format!("{slug}-{n}")
        };
        let branch = format!("cowboy/{suffixed}");
        let path = parent.join(format!("{name}-cowboy-{suffixed}"));
        if !branch_exists(repo, &branch) && !path.exists() {
            return (branch, path);
        }
    }
    unreachable!("the suffix loop always finds a free name")
}

/// Create a new worktree off `repo` for `branch` (a `cowboy/<slug>` name derived
/// from the task if `branch` is empty). Returns the chosen path and branch.
pub fn create(
    repo: &Path,
    branch: Option<&str>,
    path: Option<PathBuf>,
) -> Result<(PathBuf, String)> {
    let repo = repo_root(repo)?;
    let slug = branch
        .map(|b| b.strip_prefix("cowboy/").unwrap_or(b).to_string())
        .unwrap_or_else(|| slugify(None));
    let (branch, default_path) = plan(&repo, &slug);
    let path = path.unwrap_or(default_path);

    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "add", "-b", &branch])
        .arg(&path)
        .output()
        .context("running git worktree add")?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok((path, branch))
}

/// List the repo's worktrees (path + branch). Status is filled in by the daemon
/// from its session registry; here it defaults to `Idle`.
pub fn list(repo: &Path) -> Result<Vec<WorktreeInfo>> {
    let repo = repo_root(repo)?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("running git worktree list")?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut list = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut flush = |path: &mut Option<PathBuf>, branch: &mut Option<String>| {
        if let Some(p) = path.take() {
            list.push(WorktreeInfo {
                session: None,
                branch: branch.take().unwrap_or_else(|| "(detached)".into()),
                path: p,
                status: SessionStatus::Idle,
            });
        }
    };
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch);
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            // "branch refs/heads/foo" -> "foo"
            branch = Some(b.trim_start_matches("refs/heads/").to_string());
        }
    }
    flush(&mut path, &mut branch);
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn init_repo() -> assert_fs::TempDir {
        let dir = assert_fs::TempDir::new().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "t@t"]);
        git(dir.path(), &["config", "user.name", "t"]);
        std::fs::write(dir.path().join("README"), "hi").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "init"]);
        dir
    }

    #[test]
    fn slugify_is_clean_and_bounded() {
        assert_eq!(slugify(Some("Fix the Tests!")), "fix-the-tests");
        assert_eq!(slugify(Some("  --weird-- ")), "weird");
        assert_eq!(slugify(None), "work");
        assert_eq!(slugify(Some("")), "work");
        assert_eq!(slugify(Some("a".repeat(100).as_str())).len(), 40);
    }

    #[test]
    fn create_then_list_includes_the_new_worktree() {
        let repo = init_repo();
        let (path, branch) = create(repo.path(), Some("cowboy/feature-x"), None).unwrap();
        assert!(path.exists(), "worktree dir should exist");
        assert_eq!(branch, "cowboy/feature-x");

        let worktrees = list(repo.path()).unwrap();
        assert!(
            worktrees.iter().any(|w| w.branch == "cowboy/feature-x"),
            "list should include the new branch: {worktrees:?}"
        );
    }

    #[test]
    fn create_suffixes_on_collision() {
        let repo = init_repo();
        let (_p1, b1) = create(repo.path(), Some("cowboy/dup"), None).unwrap();
        let (_p2, b2) = create(repo.path(), Some("cowboy/dup"), None).unwrap();
        assert_eq!(b1, "cowboy/dup");
        assert_eq!(b2, "cowboy/dup-2");
    }

    #[test]
    fn dirty_base_is_detected() {
        let repo = init_repo();
        assert!(!is_dirty(repo.path()));
        std::fs::write(repo.path().join("README"), "changed").unwrap();
        assert!(is_dirty(repo.path()));
    }
}
