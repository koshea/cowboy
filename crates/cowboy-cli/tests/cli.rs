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
    //
    // The config half has to be complete for that to mean anything. An empty
    // `XDG_CONFIG_HOME` gives `doctor` a project with no provider, which is a *correct*
    // failure and exit 1 — so under the switch this asserted a success that could never
    // happen. It went unnoticed because nothing ran the switch until the final gate, and
    // because `providers` was only a warning until it was promoted to a failure. So the
    // home dir gets the minimum that makes the project genuinely healthy, and the test
    // now says what it claims: a freshly initialized project passes `doctor`.
    let home = assert_fs::TempDir::new().unwrap();
    let cfg = home.path().join("cowboy");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("providers.yaml"),
        "version: 1\nproviders:\n  default:\n    base_url: https://example.invalid/v1\n    \
         api_key: not-a-real-key\n",
    )
    .unwrap();
    std::fs::write(
        cfg.join("models.yaml"),
        "version: 1\ndefault: test-model\nmodels:\n  test-model:\n    provider: default\n    \
         model: vendor/test-model\n",
    )
    .unwrap();

    let assertion = cowboy()
        .current_dir(tmp.path())
        .env("XDG_CONFIG_HOME", home.path())
        .arg("doctor")
        .assert()
        .stdout(predicate::str::contains("platform"))
        .stdout(predicate::str::contains("security.yaml"))
        // The config it just wrote, plus the provider above, is a complete setup — so
        // there is nothing left telling the user to go and configure something.
        .stdout(predicate::str::contains("cowboy cannot start here yet").not());

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
fn commands_find_the_project_from_a_subdirectory() {
    // The cwd is not the project. Before this, running cowboy from `crates/foo/` found
    // no config, mounted a different workspace, and used a different session directory
    // than the same project's root — so it failed to collide with a session already
    // running there.
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(tmp.path())
        .arg("init")
        .assert()
        .success();
    let deep = tmp.path().join("crates/cli/src");
    std::fs::create_dir_all(&deep).unwrap();

    let root = tmp.path().canonicalize().unwrap();
    cowboy()
        .current_dir(&deep)
        .arg("doctor")
        .assert()
        .stdout(predicate::str::contains(root.display().to_string()));
}

#[test]
fn a_git_repo_without_config_resolves_to_the_repo_root() {
    // So `cowboy init` from a subdirectory writes at the root instead of burying config
    // three levels down.
    let tmp = assert_fs::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
    let deep = tmp.path().join("a/b");
    std::fs::create_dir_all(&deep).unwrap();

    cowboy().current_dir(&deep).arg("init").assert().success();
    assert!(
        tmp.path().join(".cowboy/security.yaml").is_file(),
        "config should land at the repo root"
    );
    assert!(!deep.join(".cowboy").exists(), "not in the subdirectory");
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
