//! `cowboy down` — stop and remove cowboy-managed containers and networks.

use anyhow::Result;

use crate::cli::DownArgs;
use crate::net::docker::{CliDocker, DockerCli};
use crate::net::{gateway, runtime};

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
    let hash = runtime::project_hash(&root);
    let agent = std::env::var("COWBOY_CONTAINER_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| runtime::container_name_for(&root));
    let (internal, egress, gw) = gateway::network_names(hash);

    for c in [&agent, &gw] {
        let _ = docker.remove(c, true).await;
    }
    for n in [&internal, &egress] {
        let _ = docker.remove_network(n).await;
    }
    println!("cowboy down: removed this project's containers and networks");
    Ok(())
}
