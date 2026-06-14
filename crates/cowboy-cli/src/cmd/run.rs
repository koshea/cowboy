//! `cowboy run <command>` and `cowboy shell` — execute inside the agent
//! container.

use std::process::exit;

use anyhow::{Context, Result};
use cowboy_core::config::{ConfigPaths, SecurityConfig};

use crate::net::docker::CliDocker;
use crate::net::runtime::AgentRuntime;

/// Build an [`AgentRuntime`] backed by the real `docker` CLI for this project.
fn runtime() -> Result<AgentRuntime> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);
    let security = SecurityConfig::load(&paths.security)
        .context("loading .cowboy/security.yaml (run `cowboy init` first)")?;
    Ok(AgentRuntime::new(
        Box::new(CliDocker::new()),
        root,
        security,
    ))
}

pub async fn run(command: Vec<String>) -> Result<()> {
    let rt = runtime()?;
    let result = rt.run(&command).await?;
    // Propagate the command's exit code to our caller.
    if result.exit_code != 0 {
        exit(result.exit_code);
    }
    Ok(())
}

pub async fn shell() -> Result<()> {
    let rt = runtime()?;
    let result = rt.shell().await?;
    if result.exit_code != 0 {
        exit(result.exit_code);
    }
    Ok(())
}
