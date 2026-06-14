//! `cowboy worktree list|create` — manage git worktrees for parallel sessions.
//!
//! The daemon does the git work (so listings can be annotated with the session
//! occupying each worktree). Worktrees and branches are never deleted here.

use anyhow::{Context, Result};
use cowboy_core::daemonproto::{DaemonReq, DaemonResp};

use crate::cmd::daemon;

/// `cowboy worktree list`.
pub async fn list() -> Result<()> {
    let repo = crate::cmd::project_root()?;
    daemon::ensure_running().await?;
    let resp = daemon::request(DaemonReq::ListWorktrees { repo })
        .await
        .context("listing worktrees via cowboyd")?;
    let list = match resp {
        DaemonResp::Worktrees { list } => list,
        DaemonResp::Err { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected daemon response: {other:?}"),
    };
    if list.is_empty() {
        println!("no worktrees");
        return Ok(());
    }
    println!("{:<24} {:<28} SESSION", "BRANCH", "PATH");
    for w in &list {
        println!(
            "{:<24} {:<28} {}",
            w.branch,
            w.path.display(),
            w.session.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// `cowboy worktree create [NAME]`.
pub async fn create(name: Option<String>) -> Result<()> {
    let repo = crate::cmd::project_root()?;
    if crate::net::worktree::is_dirty(&repo) {
        println!(
            "warning: uncommitted changes won't carry into the new worktree \
             (it checks out the last commit)."
        );
    }
    daemon::ensure_running().await?;
    let branch = format!("cowboy/{}", crate::net::worktree::slugify(name.as_deref()));
    let resp = daemon::request(DaemonReq::CreateWorktree {
        repo,
        branch,
        path: None,
    })
    .await
    .context("creating worktree via cowboyd")?;
    match resp {
        DaemonResp::WorktreeCreated { path, branch } => {
            println!("created worktree {} on {branch}", path.display());
            println!(
                "start a session there with:  cowboy --new-worktree  (or cd {} && cowboy)",
                path.display()
            );
            Ok(())
        }
        DaemonResp::Err { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected daemon response: {other:?}"),
    }
}
