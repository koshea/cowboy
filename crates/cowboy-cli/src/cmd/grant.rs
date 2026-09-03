//! `cowboy grant` — let the sandbox see a host path outside the project.
//!
//! The container's mounts were fixed when it started, so reaching a sibling
//! repository meant editing `security.yaml` and restarting. Here a grant is just an
//! entry in the bind list the *next* command is built from, so this takes effect
//! immediately — including in a session that is already running, with no message to
//! the worker: the sandbox rebuilds its plan per command and re-reads the store.
//!
//! Grants are stored host-side (see [`crate::sandbox::grants`]) and re-checked
//! against the credential denylist every time they are used, so writing one here
//! cannot hand the agent a credential store even if this command were tricked into
//! recording it.

use std::path::Path;

use anyhow::{Context, Result};
use cowboy_sandbox::plan::Grant;
use cowboy_sandbox::Denylist;

use crate::cli::GrantArgs;
use crate::cmd::sandbox::RealHost;
use crate::sandbox::grants::{self, Persistence};

pub fn run(args: GrantArgs) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let dir = grants::dir();

    if args.list {
        return list(&dir, &root);
    }

    let Some(raw) = args.path.as_deref() else {
        anyhow::bail!("give a path to grant, or `--list` to see the current ones");
    };

    if args.remove {
        let path = cowboy_core::config::expand_path(raw)
            .with_context(|| format!("understanding the path `{raw}`"))?;
        // Match on the resolved path when it still exists, so `--remove` accepts the
        // same spelling that `grant` accepted; fall back to the literal path so a
        // grant for something since deleted can still be cleared.
        let resolved = std::fs::canonicalize(&path).unwrap_or(path);
        let changed = grants::remove_in(&dir, &root, &resolved)
            .with_context(|| format!("forgetting the grant for {}", resolved.display()))?;
        if changed {
            println!("forgot the grant for {}", resolved.display());
            println!("Running commands keep it until the session restarts.");
        } else {
            println!("no saved grant for {}", resolved.display());
        }
        return Ok(());
    }

    let path = cowboy_core::config::expand_path(raw)
        .with_context(|| format!("understanding the path `{raw}`"))?;
    // Resolve before recording: a grant is checked against the denylist by real
    // path, and recording a symlink would let what it points at change afterwards.
    let path = std::fs::canonicalize(&path).with_context(|| {
        format!(
            "{} does not exist — a grant cannot create it",
            path.display()
        )
    })?;

    // The same check the sandbox applies when it builds a plan. Done here too so the
    // user gets an error now, rather than a grant that is silently ignored later.
    let denylist = Denylist::build(&RealHost, &root);
    if let Some(reason) = denylist.check(&path) {
        anyhow::bail!("{}", reason.explain());
    }

    let persistence = if args.global {
        Persistence::Global
    } else {
        Persistence::Project
    };
    let grant = Grant {
        path: path.clone(),
        read_only: args.ro,
    };
    let changed = grants::add_in(&dir, &root, &grant, persistence)
        .with_context(|| format!("saving the grant for {}", path.display()))?;

    let access = if args.ro { "read-only" } else { "read-write" };
    if changed {
        println!(
            "granted {access} access to {} for {}",
            path.display(),
            persistence.label()
        );
        println!("It applies to the next command, in this session and future ones.");
        println!("Already-running processes keep their old view until restarted.");
    } else {
        println!(
            "{} was already granted {access} for {}",
            path.display(),
            persistence.label()
        );
    }
    Ok(())
}

fn list(dir: &Path, root: &Path) -> Result<()> {
    let entries = grants::listing(dir, root);
    if entries.is_empty() {
        println!("no saved grants for {}", root.display());
        println!("Add one with `cowboy grant <path>`.");
        return Ok(());
    }
    // Flag anything the denylist now refuses. A grant can become invalid after it was
    // saved — a global grant naming `~/work` is fine until a project keeps
    // credentials there — and it would otherwise be silently ignored at run time.
    let denylist = Denylist::build(&RealHost, root);
    println!("saved grants for {}\n", root.display());
    for (grant, scope) in entries {
        let access = if grant.read_only { "ro" } else { "rw" };
        let note = match denylist.check(&grant.path) {
            Some(reason) => format!("  REFUSED: {}", reason.explain()),
            None if !grant.path.exists() => "  (path no longer exists)".to_string(),
            None => String::new(),
        };
        println!(
            "  {:2}  {:12}  {}{}",
            access,
            scope.label(),
            grant.path.display(),
            note
        );
    }
    Ok(())
}
