//! `cowboy agents list|show <name>` — discover and read agent definitions.
//!
//! Mirrors `cowboy skill`, but for specialist agent personas under
//! `.claude/agents/` or `.cowboy/agents/`. The foreman invokes these through
//! `shell` to discover reviewers/specialists, then delegates with the
//! `subagent` tool's `agent: <name>` option to adopt one.

use anyhow::{bail, Result};
use cowboy_core::agents;

use crate::cli::{AgentsArgs, AgentsCommand};

pub fn run(args: AgentsArgs) -> Result<()> {
    let root = crate::cmd::project_root()?;
    match args.command {
        AgentsCommand::List => list(&root),
        AgentsCommand::Show { name } => show(&root, &name),
    }
}

fn list(root: &std::path::Path) -> Result<()> {
    let agents = agents::discover(root);
    if agents.is_empty() {
        println!("no agents found (add `.claude/agents/<name>.md` or `.cowboy/agents/<name>.md`)");
        return Ok(());
    }
    for a in agents {
        let scope = if a.global { " (global)" } else { "" };
        let model = a.model.map(|m| format!(" [{m}]")).unwrap_or_default();
        // Agent descriptions are often multi-line/multi-example; show the first
        // line so the list stays one row per agent.
        let desc = a.description.lines().next().unwrap_or("").trim();
        println!("{:<28}{model}{scope}  {desc}", a.name);
    }
    Ok(())
}

fn show(root: &std::path::Path, name: &str) -> Result<()> {
    match agents::load(root, name) {
        Some(agent) => {
            println!("{}", agent.instructions);
            Ok(())
        }
        None => {
            let available: Vec<_> = agents::discover(root).into_iter().map(|a| a.name).collect();
            bail!("no agent named {name:?}; available: {available:?}");
        }
    }
}
