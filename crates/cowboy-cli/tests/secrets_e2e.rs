//! End-to-end proof that a host credential grant is mounted into the agent
//! container read-only, and that the host file is never modified.
//!
//! Brings up a real (non-isolated) agent container with a `secrets.files` grant
//! and asserts via `cowboy run`:
//!   * the granted file is readable at its container target,
//!   * the mount is read-only (a write inside the container fails),
//!   * the host file is byte-for-byte unchanged afterwards.
//!
//! Marked `#[ignore]`: it runs a real container, so it is opt-in
//! (`cargo test -- --ignored secrets`). Skips if Docker is absent.

use std::collections::HashSet;
use std::process::Command as Std;

use assert_cmd::Command;
use assert_fs::prelude::*;

fn docker_available() -> bool {
    Std::new("docker")
        .args(["info"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn cowboy_objects() -> HashSet<String> {
    Std::new("docker")
        .args(["ps", "-aq", "--filter", "label=cowboy=1"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "runs a real container with a credential mount"]
fn credential_grant_is_mounted_read_only_and_host_is_unchanged() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }

    // A host credential file with known contents, granted into the container.
    let host = assert_fs::TempDir::new().unwrap();
    let cred = host.child("token");
    cred.write_str("s3cr3t-token\n").unwrap();
    let cred_path = cred.path().to_string_lossy().into_owned();

    let tmp = assert_fs::TempDir::new().unwrap();
    let xdg = assert_fs::TempDir::new().unwrap(); // isolate the user overlay

    // Non-isolated (no gateway) busybox agent with a single read-only grant.
    tmp.child(".cowboy/security.yaml")
        .write_str(&format!(
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
             \x20   enabled: false\n\
             network_policy:\n\
             \x20 default_external: deny\n\
             secrets:\n\
             \x20 files:\n\
             \x20   - source: {cred_path}\n\
             \x20     target: /tmp/cred/token\n\
             \x20     read_only: true\n",
        ))
        .unwrap();
    tmp.child(".cowboy/agent.yaml")
        .write_str("version: 1\n")
        .unwrap();

    let agent_name = format!("cowboy-secrets-e2e-{}", std::process::id());
    let before = cowboy_objects();

    let cowboy = |args: &[&str]| -> std::process::Output {
        Command::cargo_bin("cowboy")
            .unwrap()
            .current_dir(tmp.path())
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("COWBOY_CONTAINER_NAME", &agent_name)
            .args(args)
            .output()
            .unwrap()
    };

    // 1. The credential is readable at its container target.
    let read = cowboy(&["run", "cat", "/tmp/cred/token"]);
    let read_out = String::from_utf8_lossy(&read.stdout).into_owned();

    // 2. The mount is read-only: writing to it inside the container fails.
    let write_blocked = !cowboy(&["run", "sh", "-c", "echo x > /tmp/cred/token"])
        .status
        .success();

    // Cleanup BEFORE asserting so a failure never leaks the container.
    let _ = Std::new("docker").args(["rm", "-f", &agent_name]).output();
    for id in cowboy_objects().difference(&before) {
        let _ = Std::new("docker").args(["rm", "-f", id]).output();
    }

    // 3. The host file is byte-for-byte unchanged.
    let host_after = std::fs::read_to_string(cred.path()).unwrap();

    assert!(
        read.status.success() && read_out.contains("s3cr3t-token"),
        "granted credential should be readable in the container (got: {read_out:?})"
    );
    assert!(
        write_blocked,
        "a read-only credential mount must reject writes inside the container"
    );
    assert_eq!(
        host_after, "s3cr3t-token\n",
        "the host credential file must be unmodified"
    );
}
