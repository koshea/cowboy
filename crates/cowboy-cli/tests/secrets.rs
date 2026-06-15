//! CLI tests for `cowboy secrets`. `add` writes the personal home-dir overlay by
//! default (so XDG_CONFIG_HOME is isolated), or prints a repo snippet with
//! `--repo`. `list` shows the merged view (repo + user global + user per-project).

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

#[test]
fn add_repo_prints_grant_and_network_domains() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["secrets", "add", "gh", "--repo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("source: ~/.config/gh"))
        .stdout(predicate::str::contains("target: /tmp/.config/gh"))
        .stdout(predicate::str::contains("read_only: true"))
        .stdout(predicate::str::contains("api.github.com"));
}

#[test]
fn add_writes_personal_per_project_overlay() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["secrets", "add", "gh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("this repo (all worktrees)"));

    // It shows up as a user-project grant in the merged listing.
    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("~/.config/gh → /tmp/.config/gh"))
        .stdout(predicate::str::contains("[user-project]"));
}

#[test]
fn add_global_writes_the_global_overlay() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["secrets", "add", "gh", "--global"])
        .assert()
        .success()
        .stdout(predicate::str::contains("all projects"));

    home.child("cowboy/secrets/global.yaml")
        .assert(predicate::str::contains("source: ~/.config/gh"))
        .assert(predicate::str::contains("api.github.com"));
}

#[test]
fn add_unknown_preset_fails() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["secrets", "add", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown preset"));
}

#[test]
fn list_labels_repo_grants() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    proj.child(".cowboy/security.yaml")
        .write_str(
            "version: 1\nsecrets:\n  files:\n    - source: /no/such/cred\n      target: /tmp/.config/gh\n      read_only: true\n",
        )
        .unwrap();
    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/no/such/cred → /tmp/.config/gh"))
        .stdout(predicate::str::contains("[repo]"))
        .stdout(predicate::str::contains("MISSING on host"));
}
