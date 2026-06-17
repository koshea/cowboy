//! `cowboy down` — stop and remove cowboy-managed containers and networks.

use std::path::Path;

use anyhow::Result;

use crate::cli::DownArgs;
use crate::net::docker::{CliDocker, DockerCli};
use crate::net::{gateway, runtime};

/// Remove a single project's cowboy objects: its agent container, its gateway
/// container, and its internal/egress networks (all named deterministically from
/// the worktree `root`). Best-effort — missing objects are ignored. Shared by
/// `cowboy down`, the worker's clean-shutdown reap, and the daemon's crash reap,
/// so they all tear down exactly the same set.
pub async fn teardown_project(docker: &dyn DockerCli, root: &Path, container_name: &str) {
    let hash = runtime::project_hash(root);
    let (internal, egress, gw) = gateway::network_names(hash);
    for c in [container_name, &gw] {
        let _ = docker.remove(c, true).await;
    }
    for n in [&internal, &egress] {
        let _ = docker.remove_network(n).await;
    }
}

pub async fn run(args: DownArgs) -> Result<()> {
    let docker = CliDocker::new();

    if args.all {
        let (containers, networks) = docker.list_labeled().await?;
        for c in &containers {
            let _ = docker.remove(c, true).await;
        }
        for n in &networks {
            let _ = docker.remove_network(n).await;
        }
        println!(
            "removed {} container(s) and {} network(s)",
            containers.len(),
            networks.len()
        );
        return Ok(());
    }

    // This project's deterministic names.
    let root = crate::cmd::project_root()?;
    let agent = std::env::var("COWBOY_CONTAINER_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| runtime::container_name_for(&root));

    teardown_project(&docker, &root, &agent).await;
    println!("cowboy down: removed this project's containers and networks");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::docker::MockDockerCli;

    #[tokio::test]
    async fn teardown_removes_agent_gateway_and_both_networks() {
        let root = std::path::Path::new("/tmp/cowboy-teardown-test");
        let (internal, egress, gw) = gateway::network_names(runtime::project_hash(root));
        let agent = "cowboy-agent-test".to_string();

        let mut docker = MockDockerCli::new();
        let (a, g) = (agent.clone(), gw.clone());
        docker
            .expect_remove()
            .times(2)
            .withf(move |name, force| *force && (name == a || name == g))
            .returning(|_, _| Ok(()));
        let (i, e) = (internal.clone(), egress.clone());
        docker
            .expect_remove_network()
            .times(2)
            .withf(move |n| n == i || n == e)
            .returning(|_| Ok(()));

        teardown_project(&docker, root, &agent).await;
    }
}
