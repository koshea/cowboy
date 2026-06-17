//! `cowboy decisions` — list/show recorded decisions for a session.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cowboy_core::decision;

use crate::cli::DecisionsCommand;

pub fn run(command: DecisionsCommand) -> Result<()> {
    let root = crate::cmd::project_root()?;
    match command {
        DecisionsCommand::List { session } => list(&root, session.as_deref()),
        DecisionsCommand::Show { id, session } => show(&root, session.as_deref(), &id),
    }
}

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
    let decisions = decision::list_in(&dir);
    if decisions.is_empty() {
        println!("no decisions recorded in this session");
        return Ok(());
    }
    for d in &decisions {
        println!(
            "{}  {}  → {}",
            d.id,
            d.question,
            d.selected.as_deref().unwrap_or("(no answer)")
        );
    }
    Ok(())
}

fn show(root: &Path, session: Option<&str>, id: &str) -> Result<()> {
    let dir = session_dir(root, session)?;
    match decision::get_in(&dir, id) {
        Some(d) => {
            println!("Decision {}", d.id);
            println!("  question:  {}", d.question);
            if !d.options.is_empty() {
                println!("  options:   {}", d.options.join(", "));
            }
            println!("  selected:  {}", d.selected.as_deref().unwrap_or("(none)"));
            if let Some(r) = &d.rationale {
                println!("  rationale: {r}");
            }
            Ok(())
        }
        None => bail!("no decision `{id}` in this session"),
    }
}
