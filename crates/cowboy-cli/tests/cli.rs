//! CLI integration tests for `cowboy init` and `cowboy doctor`.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

#[test]
fn help_lists_commands() {
    cowboy()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("patch"));
}

#[test]
fn init_creates_config_files() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let home = assert_fs::TempDir::new().unwrap(); // isolated home config
    cowboy()
        .current_dir(tmp.path())
        .env("XDG_CONFIG_HOME", home.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized cowboy config"))
        // With no provider configured yet, init points at setup.
        .stdout(predicate::str::contains("cowboy models setup"));

    tmp.child(".cowboy/security.yaml")
        .assert(predicate::path::is_file());
    tmp.child(".cowboy/agent.yaml")
        .assert(predicate::path::is_file());
    // Provider credentials are host-owned; no models.yaml in the project.
    tmp.child(".cowboy/models.yaml")
        .assert(predicate::path::missing());
    tmp.child(".gitignore")
        .assert(predicate::str::contains(".cowboy/sessions/"));
}

#[test]
fn init_is_idempotent_without_force() {
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success();
    cowboy()
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("skip"));
}

#[test]
fn doctor_runs_after_init() {
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success();
    // Doctor should succeed on this host (Linux + docker + nft present), though
    // it may warn about a missing provider.
    let home = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(tmp.path())
        .env("XDG_CONFIG_HOME", home.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("platform"))
        .stdout(predicate::str::contains("security.yaml"));
}

#[test]
fn doctor_fails_without_config() {
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(tmp.path())
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicate::str::contains("run `cowboy init`"));
}

#[test]
fn run_without_init_gives_clear_guidance() {
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(tmp.path())
        .args(["run", "pwd"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cowboy init"));
}

#[test]
fn logs_on_empty_project_reports_no_sessions() {
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(tmp.path())
        .arg("logs")
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions"));
}

#[test]
fn replay_unknown_session_errors() {
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(tmp.path())
        .args(["replay", "does-not-exist"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such session"));
}
