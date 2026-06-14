//! Tests for `cowboy down`.

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

#[test]
fn down_succeeds_when_nothing_running() {
    let tmp = assert_fs::TempDir::new().unwrap();
    Command::cargo_bin("cowboy")
        .unwrap()
        .current_dir(tmp.path())
        .arg("down")
        .assert()
        .success()
        .stdout(predicate::str::contains("removed this project's"));
}

#[test]
fn down_removes_a_started_container() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }
    let tmp = assert_fs::TempDir::new().unwrap();
    tmp.child(".cowboy/security.yaml")
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
    let name = format!("cowboy-down-e2e-{}", std::process::id());

    // Start the agent container via `cowboy run`.
    Command::cargo_bin("cowboy")
        .unwrap()
        .current_dir(tmp.path())
        .env("COWBOY_CONTAINER_NAME", &name)
        .args(["run", "true"])
        .assert()
        .success();
    let exists = |n: &str| {
        Std::new("docker")
            .args(["inspect", n])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    assert!(exists(&name), "container should exist after run");

    // `cowboy down` removes it.
    Command::cargo_bin("cowboy")
        .unwrap()
        .current_dir(tmp.path())
        .env("COWBOY_CONTAINER_NAME", &name)
        .arg("down")
        .assert()
        .success();
    assert!(!exists(&name), "container should be gone after down");

    // Belt and suspenders.
    let _ = Std::new("docker").args(["rm", "-f", &name]).output();
}
