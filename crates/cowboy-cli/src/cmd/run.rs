//! `cowboy run <command>` and `cowboy shell` — execute inside the sandbox.

use std::process::exit;
use std::sync::Arc;

use anyhow::Result;

use crate::sandbox::Sandbox;

/// This project's sandbox, with no approver.
///
/// `DenyAll` is deliberate rather than incidental: these are one-off CLI paths with
/// no UI attached, so a prompt would have nobody to answer it. Saying so explicitly
/// keeps an unanswerable question from becoming an allow.
fn sandbox() -> Result<impl Sandbox> {
    let root = crate::cmd::project_root()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    crate::cmd::sandbox::open(root, Arc::new(cowboy_gateway::DenyAll))
}

pub async fn run(command: Vec<String>) -> Result<()> {
    let sb = sandbox()?;
    let result = sb.run(&command).await?;
    sb.stop().await;
    // Propagate the command's exit code to our caller.
    if result.exit_code != 0 {
        exit(result.exit_code);
    }
    Ok(())
}

pub async fn shell() -> Result<()> {
    let sb = sandbox()?;
    let result = sb.shell().await?;
    sb.stop().await;
    if result.exit_code != 0 {
        exit(result.exit_code);
    }
    Ok(())
}
