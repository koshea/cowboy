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

    // What this test is really about: after `init`, `doctor` runs and reports on the
    // platform and the project's config. Both are asserted unconditionally.
    //
    // The **exit code** is a property of the host, not of the code. `doctor` exits 1
    // when it finds a failure, which is correct on a machine that cannot sandbox — a CI
    // runner without bubblewrap, or one where unprivileged user namespaces are blocked.
    // Asserting `.success()` there tests the host. `COWBOY_SANDBOX_TESTS=required` is
    // the repo-wide switch for "the sandbox must work here", so it governs this too;
    // the two other host-capability tests (`doctor::this_host_reports_no_sandbox_failures`
    // and `preflight::the_host_meets_every_requirement`) already honour it.
    let home = assert_fs::TempDir::new().unwrap();
    let assertion = cowboy()
        .current_dir(tmp.path())
        .env("XDG_CONFIG_HOME", home.path())
        .arg("doctor")
        .assert()
        .stdout(predicate::str::contains("platform"))
        .stdout(predicate::str::contains("security.yaml"));

    if cowboy_cli::sandbox::preflight::tests_required() {
        assertion.success();
    }
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
