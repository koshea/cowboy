//! Tests for `cowboy patch` against a real temporary git repository.

use std::process::Command as Std;

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = Std::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

fn cowboy(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("cowboy").unwrap();
    c.current_dir(dir);
    c
}

/// A temp git repo with one committed file, then a modification.
fn repo_with_change() -> assert_fs::TempDir {
    let tmp = assert_fs::TempDir::new().unwrap();
    git(tmp.path(), &["init", "-q"]);
    tmp.child("file.txt").write_str("original\n").unwrap();
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-q", "-m", "init"]);
    tmp.child("file.txt").write_str("modified\n").unwrap();
    tmp
}

#[test]
fn patch_errors_outside_git_repo() {
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy(tmp.path())
        .args(["patch", "show"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a git repository"));
}

#[test]
fn patch_show_displays_diff() {
    let tmp = repo_with_change();
    cowboy(tmp.path())
        .args(["patch", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("-original"))
        .stdout(predicate::str::contains("+modified"));
}

#[test]
fn patch_save_writes_diff_file() {
    let tmp = repo_with_change();
    cowboy(tmp.path())
        .args(["patch", "save"])
        .assert()
        .success();
    tmp.child(".cowboy/diff.patch")
        .assert(predicate::str::contains("+modified"));
}

#[test]
fn patch_check_validates_stdin_patch() {
    let tmp = repo_with_change();
    // Produce a patch, revert, then check it applies cleanly.
    let diff = Std::new("git")
        .args(["diff"])
        .current_dir(tmp.path())
        .output()
        .unwrap()
        .stdout;
    git(tmp.path(), &["checkout", "--", "."]);
    cowboy(tmp.path())
        .args(["patch", "check"])
        .write_stdin(diff)
        .assert()
        .success()
        .stdout(predicate::str::contains("applies cleanly"));
}

#[test]
fn patch_revert_discards_changes_with_assume_yes() {
    let tmp = repo_with_change();
    cowboy(tmp.path())
        .args(["patch", "revert"])
        .env("COWBOY_ASSUME_YES", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("reverted"));
    // File is back to its committed content.
    tmp.child("file.txt")
        .assert(predicate::str::contains("original"));
}
