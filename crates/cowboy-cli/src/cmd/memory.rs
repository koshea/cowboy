//! `cowboy memory` — inspect/curate the agent's host-managed memory.
//!
//! Memories live under `~/.config/cowboy/memory/` (global + per-worktree). This
//! command lets a human review what the agent has stored; the agent itself reads
//! and writes memory through the `memory` tool.

use anyhow::Result;
use cowboy_core::memory;

use crate::cli::{MemoryCmdArgs, MemoryCommand};
use crate::project::project_key_hex;

/// The memory key for the current worktree (matches the agent's).
fn project_key() -> Result<String> {
    let root = crate::cmd::project_root()?;
    let canon = std::fs::canonicalize(&root).unwrap_or(root);
    Ok(project_key_hex(&canon))
}

pub fn run(args: MemoryCmdArgs) -> Result<()> {
    let key = project_key()?;
    match args.command {
        MemoryCommand::List => {
            let idx = memory::index(&key);
            if idx.is_empty() {
                println!("no memories stored");
            } else {
                print!("{idx}");
            }
        }
        MemoryCommand::Show { name } => match memory::recall(&key, &name)? {
            Some(body) => println!("{body}"),
            None => anyhow::bail!("no memory named {name:?}"),
        },
        MemoryCommand::Delete { name } => {
            // Deleting shows the body first: a memory is a few lines the agent chose to
            // keep, and there is no undo, so "is this the one?" should be answerable
            // without a second command.
            let Some(body) = memory::recall(&key, &name)? else {
                anyhow::bail!("no memory named {name:?}");
            };
            crate::ui::heading(&format!("memory `{name}`"));
            println!("{}", body.trim_end());
            if !crate::prompt::confirm_destructive(&format!("\nDelete memory `{name}`?"))? {
                return Ok(());
            }
            if memory::delete(&key, &name)? {
                crate::ui::ok(&format!("deleted memory `{name}`"));
            } else {
                anyhow::bail!("no memory named {name:?}");
            }
        }
    }
    Ok(())
}
