//! `cowboy skill list|show <name>` — discover and read agent skills.
//!
//! The agent invokes these through `shell`: `cowboy skill list` to see what is
//! available, then `cowboy skill show <name>` to pull a skill's instructions
//! into its context and follow them.

use anyhow::{bail, Result};
use cowboy_core::skills;

use crate::cli::{SkillArgs, SkillCommand};

pub fn run(args: SkillArgs) -> Result<()> {
    let root = crate::cmd::project_root()?;
    match args.command {
        SkillCommand::List => list(&root),
        SkillCommand::Show { name } => show(&root, &name),
    }
}

fn list(root: &std::path::Path) -> Result<()> {
    let skills = skills::discover(root);
    if skills.is_empty() {
        println!("no skills found (add directories under .cowboy/skills/<name>/SKILL.md)");
        return Ok(());
    }
    for s in skills {
        let scope = if s.global { " (global)" } else { "" };
        println!("{:<20} {}{scope}", s.name, s.description);
    }
    Ok(())
}

fn show(root: &std::path::Path, name: &str) -> Result<()> {
    match skills::load(root, name) {
        Some(skill) => {
            println!("{}", skill.instructions);
            Ok(())
        }
        None => {
            let available: Vec<_> = skills::discover(root).into_iter().map(|s| s.name).collect();
            bail!("no skill named {name:?}; available: {available:?}");
        }
    }
}
