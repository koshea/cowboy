//! `cowboy review` — read-only review of a session's output (or a branch).
//!
//! Assembles a review bundle — handoff, published artifacts, decisions, a
//! lifecycle summary, and the change stat — prints it, and (for a session)
//! records it as a `Review` artifact with an empty Findings section for the
//! reviewer (human or a Ranch review worker) to fill in. Never edits anything.

use std::path::Path;

use anyhow::{bail, Context, Result};
use cowboy_core::artifact::{self, ArtifactKind};
use cowboy_core::{decision, lifecycle};

pub fn run(session: Option<String>, branch: Option<String>) -> Result<()> {
    let root = crate::cmd::project_root()?;

    // Branch-only review: just the read-only change summary (no session home).
    if let Some(branch) = branch {
        let s = crate::net::worktree::status(&root, &branch)?;
        println!("# Review of branch {branch}\n");
        print!("{}", render_branch(&s));
        return Ok(());
    }

    let id = match session {
        Some(s) => s,
        None => crate::session::latest_session_id(&root)
            .context("no session given and no recent session in this worktree")?,
    };
    let dir = root.join(".cowboy").join("sessions").join(&id);
    if !dir.is_dir() {
        bail!("no such session: {id}");
    }

    let bundle = render_session(&dir, &id);
    print!("{bundle}");

    // Record the review bundle (+ a Findings stub) as a Review artifact.
    let md = format!("{bundle}\n## Findings\n(add review findings here)\n");
    match artifact::add_in(
        &dir,
        &id,
        ArtifactKind::Review,
        "Review",
        &md,
        None,
        now_ms(),
    ) {
        Ok(a) => println!("\n✓ recorded review as {} ({})", a.id, a.path.display()),
        Err(e) => eprintln!("review printed but not recorded: {e}"),
    }
    Ok(())
}

fn render_branch(s: &crate::net::worktree::WorktreeStatus) -> String {
    let mergeable = match s.mergeable {
        Some(true) => "clean",
        Some(false) => "CONFLICTS",
        None => "unknown",
    };
    let mut out = format!(
        "forked at {}; {} file(s), +{} -{}; merges {mergeable} vs HEAD\n",
        s.base,
        s.files.len(),
        s.insertions,
        s.deletions
    );
    for f in &s.files {
        out.push_str(&format!("  {f}\n"));
    }
    out
}

fn render_session(dir: &Path, id: &str) -> String {
    let mut out = format!("# Review of session {id}\n\n");

    out.push_str("## Handoff\n");
    match std::fs::read_to_string(dir.join("handoff.md")) {
        Ok(h) => {
            out.push_str(h.trim());
            out.push('\n');
        }
        Err(_) => out.push_str("(no handoff)\n"),
    }

    let artifacts = artifact::list_in(dir);
    out.push_str(&format!("\n## Artifacts ({})\n", artifacts.len()));
    for a in &artifacts {
        out.push_str(&format!("- {} [{}] {}\n", a.id, a.kind.as_str(), a.title));
    }

    let decisions = decision::list_in(dir);
    if !decisions.is_empty() {
        out.push_str(&format!("\n## Decisions ({})\n", decisions.len()));
        for d in &decisions {
            out.push_str(&format!(
                "- {}: {} → {}\n",
                d.id,
                d.question,
                d.selected.as_deref().unwrap_or("(none)")
            ));
        }
    }

    let events = lifecycle::read_in(dir);
    if !events.is_empty() {
        out.push_str(&format!("\n## Lifecycle ({} events)\n", events.len()));
    }

    if let Ok(meta) = std::fs::metadata(dir.join("diff.patch")) {
        if meta.len() > 0 {
            out.push_str(&format!("\n## Diff\ndiff.patch ({} bytes)\n", meta.len()));
        }
    }
    out
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_path(root: &Path, id: &str) -> std::path::PathBuf {
        root.join(".cowboy").join("sessions").join(id)
    }

    #[test]
    fn render_session_bundles_handoff_and_artifacts() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = session_path(tmp.path(), "s1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("handoff.md"), "# Handoff\n\n## Goal\nbuild it\n").unwrap();
        artifact::add_in(&dir, "s1", ArtifactKind::Contract, "API", "x", None, 1).unwrap();

        let bundle = render_session(&dir, "s1");
        assert!(bundle.contains("Review of session s1"));
        assert!(bundle.contains("build it"));
        assert!(bundle.contains("[contract] API"));
    }
}
