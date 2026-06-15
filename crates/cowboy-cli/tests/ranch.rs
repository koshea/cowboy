//! CLI tests for `cowboy ranch create` / `status`.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

#[test]
fn create_then_status_lists_and_shows() {
    let proj = assert_fs::TempDir::new().unwrap();

    cowboy()
        .current_dir(proj.path())
        .args(["ranch", "create", "Billing v2", "--goal", "ship billing"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created ranch `billing-v2`"));

    proj.child(".cowboy/ranches/billing-v2/ranch.yaml")
        .assert(predicate::str::contains("title: \"Billing v2\""))
        .assert(predicate::str::contains("status: planning"));

    // No-arg status lists ranches.
    cowboy()
        .current_dir(proj.path())
        .args(["ranch", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("billing-v2"))
        .stdout(predicate::str::contains("planning"));

    // A fresh ranch has no workstreams yet.
    cowboy()
        .current_dir(proj.path())
        .args(["ranch", "status", "billing-v2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no workstreams yet"));
}

#[test]
fn status_reflects_dependency_readiness() {
    let proj = assert_fs::TempDir::new().unwrap();
    // Seed a plan: schema is complete, api depends on schema, ui depends on api.
    proj.child(".cowboy/ranches/billing/ranch.yaml")
        .write_str(
            "version: 1\nid: billing\ntitle: Billing\nstatus: running\ncreated_ms: 1\nupdated_ms: 1\n\
             workstreams:\n\
             \x20 - id: schema\n    title: Schema\n    depends_on: []\n    status: complete\n\
             \x20 - id: api\n    title: API\n    depends_on: [schema]\n    status: planned\n\
             \x20 - id: ui\n    title: UI\n    depends_on: [api]\n    status: planned\n",
        )
        .unwrap();

    cowboy()
        .current_dir(proj.path())
        .args(["ranch", "status", "billing"])
        .assert()
        .success()
        // api is unblocked by the completed schema; ui stays blocked on api.
        .stdout(predicate::str::contains("ready to start: api"));
}

#[test]
fn create_suffixes_id_on_collision() {
    let proj = assert_fs::TempDir::new().unwrap();
    cowboy()
        .current_dir(proj.path())
        .args(["ranch", "create", "Billing"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created ranch `billing`"));
    cowboy()
        .current_dir(proj.path())
        .args(["ranch", "create", "Billing"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created ranch `billing-2`"));
}
