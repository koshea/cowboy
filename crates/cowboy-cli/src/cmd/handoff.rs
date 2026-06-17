//! `cowboy handoff [session]` — print a session's handoff summary.
//!
//! Every finished session has a `handoff.md` (written by the agent's `handoff`
//! tool, or auto-synthesized at finalize). Defaults to the latest session.

use anyhow::{bail, Context, Result};

pub fn run(session: Option<String>) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let id = match session {
        Some(s) => s,
        None => {
            crate::session::latest_session_id(&root).context("no sessions yet in this worktree")?
        }
    };
    let path = crate::session::session_dir(&root, &id).join("handoff.md");
    match std::fs::read_to_string(&path) {
        Ok(body) => {
            print!("{body}");
            Ok(())
        }
        Err(_) => bail!("no handoff for session {id} (not finished yet?)"),
    }
}
