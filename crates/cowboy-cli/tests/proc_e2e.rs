//! End-to-end test for the process supervisor against a real busybox container.
//! Skips when Docker is unavailable.

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

struct Project {
    dir: assert_fs::TempDir,
    container: String,
}

impl Project {
    fn new() -> Self {
        let dir = assert_fs::TempDir::new().unwrap();
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
                 networks:\n\
                 \x20 isolated:\n\
                 \x20   enabled: false\n",
            )
            .unwrap();
        dir.child(".cowboy/agent.yaml")
            .write_str(
                "version: 1\n\
                 processes:\n\
                 \x20 ticker:\n\
                 \x20   command: \"while true; do echo tick; sleep 1; done\"\n\
                 \x20   cwd: /workspace\n",
            )
            .unwrap();
        dir.child(".cowboy/models.yaml")
            .write_str("version: 1\n")
            .unwrap();
        Self {
            dir,
            container: format!("cowboy-proc-e2e-{}", std::process::id()),
        }
    }
    fn cowboy(&self, args: &[&str]) -> std::process::Output {
        Command::cargo_bin("cowboy")
            .unwrap()
            .current_dir(self.dir.path())
            .env("COWBOY_CONTAINER_NAME", &self.container)
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = Std::new("docker")
            .args(["rm", "-f", &self.container])
            .output();
    }
}

#[test]
fn proc_start_list_stop_lifecycle() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }
    let proj = Project::new();

    // Initially stopped.
    let listed = proj.cowboy(&["proc", "list"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("ticker"));

    // Start it.
    let started = proj.cowboy(&["proc", "start", "ticker"]);
    assert!(started.status.success(), "start failed: {started:?}");

    // Give it a moment, then it should be running.
    std::thread::sleep(std::time::Duration::from_millis(800));
    let running = proj.cowboy(&["proc", "list"]);
    let running_out = String::from_utf8_lossy(&running.stdout);

    // Stop it.
    let stopped = proj.cowboy(&["proc", "stop", "ticker"]);
    assert!(stopped.status.success());
    std::thread::sleep(std::time::Duration::from_millis(500));
    let after = proj.cowboy(&["proc", "list"]);
    let after_out = String::from_utf8_lossy(&after.stdout);

    // Assert after cleanup-friendly captures.
    assert!(
        running_out.contains("running"),
        "expected ticker running, got:\n{running_out}"
    );
    assert!(
        after_out.contains("stopped"),
        "expected ticker stopped, got:\n{after_out}"
    );
}

#[test]
fn proc_unknown_name_errors() {
    if !docker_available() {
        return;
    }
    let proj = Project::new();
    proj_assert_unknown(&proj);
}

fn proj_assert_unknown(proj: &Project) {
    Command::cargo_bin("cowboy")
        .unwrap()
        .current_dir(proj.dir.path())
        .env("COWBOY_CONTAINER_NAME", &proj.container)
        .args(["proc", "start", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no process named"));
}
