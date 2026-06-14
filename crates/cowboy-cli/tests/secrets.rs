//! CLI tests for `cowboy secrets` (list + add presets). `add` is non-destructive
//! (prints a paste-ready snippet), so these assert on stdout.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

#[test]
fn add_gh_prints_grant_and_network_domains() {
    let proj = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(proj.path())
        .args(["secrets", "add", "gh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("source: ~/.config/gh"))
        .stdout(predicate::str::contains("target: /tmp/.config/gh"))
        .stdout(predicate::str::contains("read_only: true"))
        .stdout(predicate::str::contains("api.github.com"));
}

#[test]
fn add_unknown_preset_fails() {
    let proj = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(proj.path())
        .args(["secrets", "add", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown preset"));
}

#[test]
fn add_explicit_env_and_file() {
    let proj = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(proj.path())
        .args([
            "secrets",
            "add",
            "--env",
            "GH_TOKEN=MY_GH_TOKEN",
            "--file",
            "/home/me/.netrc:/tmp/.netrc",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("name: GH_TOKEN"))
        .stdout(predicate::str::contains("source_env: MY_GH_TOKEN"))
        .stdout(predicate::str::contains("source: /home/me/.netrc"))
        .stdout(predicate::str::contains("target: /tmp/.netrc"));
}

#[test]
fn list_shows_a_configured_grant() {
    let proj = assert_fs::TempDir::new().unwrap();
    proj.child(".cowboy/security.yaml")
        .write_str(
            "version: 1\nsecrets:\n  files:\n    - source: /no/such/cred\n      target: /tmp/.config/gh\n      read_only: true\n",
        )
        .unwrap();
    cowboy()
        .current_dir(proj.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/no/such/cred → /tmp/.config/gh"))
        .stdout(predicate::str::contains("MISSING on host"));
}
