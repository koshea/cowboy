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
        } => add(&root, session.as_deref(), kind, &path, title, summary),
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
            let session_id = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if let Some(file) =
                viewer_session_dir(root, session_id).and_then(|d| artifact::host_file(&d, &r))
            {
                println!("{}", artifact::file_url(&file));
            }
            if let Some(s) = &r.summary {
                println!("{s}\n");
            }
            print!("{body}");
            Ok(())
        }
        None => bail!("no artifact `{id}` in this session"),
    }
}

// --- for the TUI and the web UI ----------------------------------------------

/// The session directory a viewer reads `id`'s artifacts from: under the canonical
/// `root`, and only for an id that is a single path component — the id arrives from
/// a client, and `session_dir` would happily join `../..`.
///
/// Built from the root rather than canonicalizing the session dir itself, so a
/// `.cowboy` or `sessions/<id>` the agent swapped for a symlink is not followed.
pub(crate) fn viewer_session_dir(root: &Path, id: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut parts = Path::new(id).components();
    if !matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return None;
    }
    Some(crate::session::session_dir(
        &std::fs::canonicalize(root).ok()?,
        id,
    ))
}

fn entry(dir: &Path, r: &artifact::ArtifactRef) -> cowboy_core::daemonproto::ArtifactEntry {
    cowboy_core::daemonproto::ArtifactEntry {
        id: r.id.clone(),
        kind: r.kind.as_str().to_string(),
        title: r.title.clone(),
        summary: r.summary.clone(),
        created_ms: r.created_ms,
        url: artifact::host_file(dir, r).map(|f| artifact::file_url(&f)),
    }
}

/// Session `id`'s artifacts, in publish order, each with its link when it has one.
pub fn entries(root: &Path, id: &str) -> Vec<cowboy_core::daemonproto::ArtifactEntry> {
    let Some(dir) = viewer_session_dir(root, id) else {
        return Vec::new();
    };
    artifact::list_in(&dir)
        .iter()
        .map(|r| entry(&dir, r))
        .collect()
}

/// One artifact's body for a viewer, read only from the validated host file and cut
/// at [`artifact::VIEW_MAX_BYTES`]. `None` for an unknown id or a file that is not
/// where the host put it.
pub fn body(
    root: &Path,
    id: &str,
    artifact_id: &str,
) -> Option<cowboy_core::daemonproto::ArtifactBody> {
    let dir = viewer_session_dir(root, id)?;
    let r = artifact::list_in(&dir)
        .into_iter()
        .find(|a| a.id == artifact_id)?;
    let file = artifact::host_file(&dir, &r)?;
    let (body, truncated) = artifact::read_for_view(&file)?;
    Some(cowboy_core::daemonproto::ArtifactBody {
        entry: entry(&dir, &r),
        body,
        truncated,
    })
}

fn add(
    root: &Path,
    session: Option<&str>,
    kind: Option<crate::cli::Kind>,
    path: &str,
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
        .map(crate::cli::Kind::artifact_kind)
        .unwrap_or(ArtifactKind::Notes);
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let r = artifact::add_in(&dir, &id, kind, &title, &content, summary, now_ms())?;
    crate::ui::ok(&format!(
        "published {} [{}] {} → {}",
        r.id,
        r.kind.as_str(),
        r.title,
        r.path.display()
    ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewer_lists_and_reads_a_sessions_artifacts() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = crate::session::session_dir(tmp.path(), "s1");
        std::fs::create_dir_all(&dir).unwrap();
        artifact::add_in(
            &dir,
            "s1",
            ArtifactKind::Review,
            "Review",
            "# body",
            None,
            1,
        )
        .unwrap();

        let listed = entries(tmp.path(), "s1");
        assert_eq!(listed.len(), 1);
        let url = listed[0].url.as_deref().expect("a link");
        assert!(
            url.starts_with("file:///") && url.ends_with("a0001-review.md"),
            "{url}"
        );

        let b = body(tmp.path(), "s1", "a0001").expect("a body");
        assert_eq!(b.body, "# body");
        assert!(!b.truncated);
        assert!(body(tmp.path(), "s1", "a0009").is_none());
    }

    #[test]
    fn a_session_id_from_a_client_cannot_walk_out_of_the_sessions_dir() {
        let tmp = assert_fs::TempDir::new().unwrap();
        for id in ["../..", "a/b", "/etc", ""] {
            assert!(viewer_session_dir(tmp.path(), id).is_none(), "{id:?}");
            assert!(entries(tmp.path(), id).is_empty(), "{id:?}");
        }
    }
}
