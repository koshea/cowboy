//! `cowboy down` — stop and remove cowboy-managed containers and networks.

use std::path::Path;

use anyhow::Result;
use cowboy_core::daemonproto::{DaemonReq, DaemonResp, SessionInfo};

use crate::cli::DownArgs;
use crate::net::docker::{CliDocker, DockerCli};
use crate::net::{gateway, runtime};

/// Stop the worker processes of the given live sessions (SIGTERM). A worker whose
/// container we're about to remove must be killed too — otherwise it lingers as a
/// "Running" session and recreates the container on its next command. The daemon's
/// vacuum then reaps the now-dead session records + leases. Returns the count.
fn kill_session_workers(sessions: &[SessionInfo]) -> usize {
    let mut killed = 0;
    for s in sessions {
        if s.status.is_terminal() {
            continue;
        }
        if let Some(pid) = s.pid {
            // SAFETY: kill(pid, SIGTERM) is always safe; ESRCH if already gone.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            killed += 1;
        }
    }
    killed
}

/// Live sessions known to the daemon, optionally filtered to one worktree `root`.
/// Empty if the daemon isn't running.
async fn live_sessions(root: Option<&Path>) -> Vec<SessionInfo> {
    match crate::cmd::daemon::request(DaemonReq::ListSessions {
        root: root.map(Path::to_path_buf),
    })
    .await
    {
        Ok(DaemonResp::Sessions { sessions }) => sessions,
        _ => Vec::new(),
    }
}

/// Remove a single project's cowboy objects: its agent container, its gateway
/// sidecar, and its network (all named deterministically from the worktree
/// `root`). Best-effort — missing objects are ignored. Shared by `cowboy down`,
/// the worker's clean-shutdown reap, and the daemon's crash reap, so they all
/// tear down exactly the same set. The gateway shares the agent's netns, so
/// remove it first; the network removal then succeeds.
pub async fn teardown_project(docker: &dyn DockerCli, root: &Path, container_name: &str) {
    let hash = runtime::project_hash(root);
    let (agent_net, gw) = gateway::network_names(hash);
    for c in [&gw, container_name] {
        let _ = docker.remove(c, true).await;
    }
    let _ = docker.remove_network(&agent_net).await;
}

pub async fn run(args: DownArgs) -> Result<()> {
    let docker = CliDocker::new();

    if args.all {
        // Kill every live session's worker first, so none recreates its container.
        let killed = kill_session_workers(&live_sessions(None).await);
        let (containers, networks) = docker.list_labeled().await?;
        for c in &containers {
            let _ = docker.remove(c, true).await;
        }
        for n in &networks {
            let _ = docker.remove_network(n).await;
        }
        println!(
            "stopped {killed} session(s); removed {} container(s) and {} network(s)",
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

    // End this worktree's session(s) first (kill their workers), then remove the
    // containers/networks. Without this, a still-running worker would just
    // recreate the container and the session would keep showing as Running.
    let killed = kill_session_workers(&live_sessions(Some(&root)).await);
    teardown_project(&docker, &root, &agent).await;
    println!(
        "cowboy down: stopped {killed} session(s) and removed this project's containers and networks"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::docker::MockDockerCli;

    #[tokio::test]
    async fn teardown_removes_agent_gateway_and_network() {
        let root = std::path::Path::new("/tmp/cowboy-teardown-test");
        let (agent_net, gw) = gateway::network_names(runtime::project_hash(root));
        let agent = "cowboy-agent-test".to_string();

        let mut docker = MockDockerCli::new();
        let (a, g) = (agent.clone(), gw.clone());
        docker
            .expect_remove()
            .times(2)
            .withf(move |name, force| *force && (name == a || name == g))
            .returning(|_, _| Ok(()));
        let n = agent_net.clone();
        docker
            .expect_remove_network()
            .times(1)
            .withf(move |x| x == n)
            .returning(|_| Ok(()));

        teardown_project(&docker, root, &agent).await;
    }

    #[test]
    fn kill_session_workers_skips_terminal_and_pidless() {
        // A terminal session (has a pid but already done) and a live session with
        // no pid: neither should be signalled — so nothing is killed and no real
        // process is touched.
        let mk = |status: &str, pid: serde_json::Value| -> SessionInfo {
            serde_json::from_value(serde_json::json!({
                "id": "s", "root": "/w", "status": status, "pid": pid,
            }))
            .unwrap()
        };
        let sessions = vec![
            mk("completed", serde_json::json!(999999999u32)),
            mk("running", serde_json::Value::Null),
        ];
        assert_eq!(kill_session_workers(&sessions), 0);
    }
}
