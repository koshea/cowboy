//! `cowboy artifact` — inspect and publish session artifacts.
//!
//! Artifacts are typed, titled outputs a session produces (contracts,
//! summaries, handoffs, …), stored under `.cowboy/sessions/<id>/` and indexed in
//! `artifacts.jsonl`. Commands default to the most recent session unless a
//! session id is given.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cowboy_core::artifact::{self, ArtifactKind};

use crate::cli::ArtifactCommand;

pub fn run(command: ArtifactCommand) -> Result<()> {
    let root = crate::cmd::project_root()?;
    match command {
        ArtifactCommand::List { session } => list(&root, session.as_deref()),
        ArtifactCommand::Show { id, session } => show(&root, session.as_deref(), &id),
        ArtifactCommand::Add {
            path,
            kind,
            title,
            summary,
            session,
        } => add(&root, session.as_deref(), &path, kind, title, summary),
    }
}

/// Resolve a session directory by id, or the latest session when none given.
fn session_dir(root: &Path, session: Option<&str>) -> Result<PathBuf> {
    let id = match session {
        Some(s) => s.to_string(),
        None => {
            crate::session::latest_session_id(root).context("no sessions yet in this worktree")?
        }
    };
    let dir = crate::session::session_dir(root, &id);
    if !dir.is_dir() {
        bail!("no such session: {id}");
    }
    Ok(dir)
}

fn list(root: &Path, session: Option<&str>) -> Result<()> {
    let dir = session_dir(root, session)?;
    let arts = artifact::list_in(&dir);
    if arts.is_empty() {
        println!("no artifacts in this session");
        return Ok(());
    }
    println!("{:<7} {:<14} {:<28} SUMMARY", "ID", "KIND", "TITLE");
    for a in &arts {
        println!(
            "{:<7} {:<14} {:<28} {}",
            a.id,
            a.kind.as_str(),
            truncate(&a.title, 28),
            a.summary.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn show(root: &Path, session: Option<&str>, id: &str) -> Result<()> {
    let dir = session_dir(root, session)?;
    match artifact::get_in(&dir, id) {
        Some((r, body)) => {
            println!(
                "# {} [{}]  ({})",
                r.title,
                r.kind.as_str(),
                r.path.display()
            );
            if let Some(s) = &r.summary {
                println!("{s}\n");
            }
            print!("{body}");
            Ok(())
        }
        None => bail!("no artifact `{id}` in this session"),
    }
}

fn add(
    root: &Path,
    session: Option<&str>,
    path: &str,
    kind: Option<String>,
    title: Option<String>,
    summary: Option<String>,
) -> Result<()> {
    let dir = session_dir(root, session)?;
    let content = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let title = title.unwrap_or_else(|| {
        Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".into())
    });
    let kind = kind
        .as_deref()
        .map(ArtifactKind::parse)
        .unwrap_or(ArtifactKind::Notes);
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let r = artifact::add_in(&dir, &id, kind, &title, &content, summary, now_ms())?;
    println!(
        "✓ published {} [{}] {} → {}",
        r.id,
        r.kind.as_str(),
        r.title,
        r.path.display()
    );
    Ok(())
}

use cowboy_core::time::now_ms;

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
