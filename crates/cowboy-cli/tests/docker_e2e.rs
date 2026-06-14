//! End-to-end Docker tests for `cowboy run`, exercising the real `docker` CLI.
//!
//! These prove the Slice B acceptance criteria:
//!   * `cowboy run pwd` -> `/workspace`
//!   * the host-owned `security.yaml` is masked (unreadable) inside the container
//!
//! They use the tiny `busybox` image and skip cleanly when Docker is
//! unavailable, so the suite stays green on machines without a daemon.

use std::process::Command as Std;

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn docker_available() -> bool {
    Std::new("docker")
        .args(["info"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn unique_name() -> String {
    // Avoid pulling in uuid here; pid + nanos is unique enough for a test run.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cowboy-e2e-{}-{}", std::process::id(), nanos)
}

fn cleanup(name: &str) {
    let _ = Std::new("docker").args(["rm", "-f", name]).output();
}

struct Project {
    dir: assert_fs::TempDir,
    container: String,
}

impl Project {
    fn new() -> Self {
        let dir = assert_fs::TempDir::new().unwrap();
        // security.yaml: busybox image, project mounted at /workspace.
        dir.child(".cowboy/security.yaml")
            .write_str(
                "version: 1\n\
                 container:\n\
                 \x20 image: busybox:latest\n\
                 \x20 workdir: /workspace\n\
                 \x20 mounts:\n\
                 \x20   - source: .\n\
                 \x20     target: /workspace\n\
                 \x20     mode: rw\n\
                 \x20 memory: null\n\
                 networks:\n\
                 \x20 isolated:\n\
                 \x20   enabled: false\n",
            )
            .unwrap();
        dir.child(".cowboy/agent.yaml")
            .write_str("version: 1\n")
            .unwrap();
        dir.child(".cowboy/models.yaml")
            .write_str("version: 1\n")
            .unwrap();
        Self {
            dir,
            container: unique_name(),
        }
    }

    fn cowboy(&self) -> Command {
        let mut cmd = Command::cargo_bin("cowboy").unwrap();
        cmd.current_dir(self.dir.path())
            .env("COWBOY_CONTAINER_NAME", &self.container);
        cmd
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        cleanup(&self.container);
    }
}

#[test]
fn run_pwd_reports_workspace() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }
    let proj = Project::new();
    proj.cowboy()
        .args(["run", "pwd"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/workspace"));
}

#[test]
fn agent_runs_as_host_uid_not_root() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }
    let proj = Project::new();
    // The container must run as the host uid:gid (non-root), so files the agent
    // writes are owned by the user and it never has root inside the workspace.
    let uid = unsafe { libc::getuid() };
    proj.cowboy()
        .args(["run", "id", "-u"])
        .assert()
        .success()
        .stdout(predicate::str::contains(uid.to_string()));
}

#[test]
fn security_yaml_is_masked_inside_container() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }
    let proj = Project::new();

    // The host file has real content...
    let host = std::fs::read_to_string(proj.dir.path().join(".cowboy/security.yaml")).unwrap();
    assert!(host.contains("busybox"));

    // ...but inside the container it must be empty (masked by the empty file).
    proj.cowboy()
        .args(["run", "cat", "/workspace/.cowboy/security.yaml"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    // agent.yaml, by contrast, IS visible to the agent.
    proj.cowboy()
        .args(["run", "cat", "/workspace/.cowboy/agent.yaml"])
        .assert()
        .success()
        .stdout(predicate::str::contains("version: 1"));
}
