//! CLI tests for `cowboy skill list|show`.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("cowboy").unwrap();
    c.current_dir(dir);
    // Isolate from the real home so global `~/.config/cowboy/skills` and
    // `~/.claude/skills` don't leak into these project-scoped tests.
    c.env("HOME", dir);
    c.env("XDG_CONFIG_HOME", dir);
    c
}

fn project_with_skill() -> assert_fs::TempDir {
    let tmp = assert_fs::TempDir::new().unwrap();
    tmp.child(".cowboy/skills/run-tests/SKILL.md")
        .write_str("---\nname: run-tests\ndescription: build and test\n---\nStep 1: cargo test\n")
        .unwrap();
    tmp
}

#[test]
fn skill_list_shows_skills() {
    let tmp = project_with_skill();
    cowboy(tmp.path())
        .args(["skill", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("run-tests"))
        .stdout(predicate::str::contains("build and test"));
}

#[test]
fn skill_show_prints_instructions() {
    let tmp = project_with_skill();
    cowboy(tmp.path())
        .args(["skill", "show", "run-tests"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Step 1: cargo test"));
}

#[test]
fn skill_show_unknown_errors() {
    let tmp = project_with_skill();
    cowboy(tmp.path())
        .args(["skill", "show", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no skill named"));
}

#[test]
fn skill_list_empty_project() {
    let tmp = assert_fs::TempDir::new().unwrap();
    cowboy(tmp.path())
        .args(["skill", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no skills found"));
}
