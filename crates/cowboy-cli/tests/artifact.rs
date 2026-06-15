//! CLI tests for `cowboy artifact` (add/list/show) against a seeded session dir.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

#[test]
fn add_then_list_and_show() {
    let proj = assert_fs::TempDir::new().unwrap();
    // Seed a session directory + LATEST pointer (as a worker would).
    proj.child(".cowboy/sessions/sess1")
        .create_dir_all()
        .unwrap();
    proj.child(".cowboy/sessions/LATEST")
        .write_str("sess1")
        .unwrap();
    proj.child("api-contract.md")
        .write_str("# API\nGET /things\n")
        .unwrap();

    // Publish (defaults to the latest session).
    cowboy()
        .current_dir(proj.path())
        .args([
            "artifact",
            "add",
            "api-contract.md",
            "--kind",
            "contract",
            "--title",
            "API Contract",
            "--summary",
            "the billing API",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("published a0001"))
        .stdout(predicate::str::contains("[contract]"));

    // List shows it.
    cowboy()
        .current_dir(proj.path())
        .args(["artifact", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("a0001"))
        .stdout(predicate::str::contains("API Contract"))
        .stdout(predicate::str::contains("the billing API"));

    // Show prints the body.
    cowboy()
        .current_dir(proj.path())
        .args(["artifact", "show", "a0001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("GET /things"));
}

#[test]
fn list_with_no_session_is_clear() {
    let proj = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(proj.path())
        .args(["artifact", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no sessions yet"));
}
