//! CLI test for `cowboy review` against a seeded session dir.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

#[test]
fn review_prints_bundle_and_records_artifact() {
    let proj = assert_fs::TempDir::new().unwrap();
    proj.child(".cowboy/sessions/s1").create_dir_all().unwrap();
    proj.child(".cowboy/sessions/LATEST")
        .write_str("s1")
        .unwrap();
    proj.child(".cowboy/sessions/s1/handoff.md")
        .write_str("# Handoff\n\n## Goal\nimplement billing\n")
        .unwrap();

    cowboy()
        .current_dir(proj.path())
        .args(["review", "s1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Review of session s1"))
        .stdout(predicate::str::contains("implement billing"))
        .stdout(predicate::str::contains("recorded review as a0001"));

    // The review was recorded as a Review artifact.
    cowboy()
        .current_dir(proj.path())
        .args(["artifact", "list", "s1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("review"))
        .stdout(predicate::str::contains("a0001"));
}
