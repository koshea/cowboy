//! CLI tests for `cowboy memory`, with the home config dir isolated via
//! `XDG_CONFIG_HOME`. Global memories don't depend on the project key, so we seed
//! one and read it back.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

fn seed_global(home: &assert_fs::TempDir) {
    home.child("cowboy/memory/global/pref.md")
        .write_str(
            "---\nname: pref\ndescription: prefers tabs\nscope: global\ntype: preference\n---\nUse tabs, not spaces.\n",
        )
        .unwrap();
}

#[test]
fn list_shows_seeded_global_memory() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_global(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pref"))
        .stdout(predicate::str::contains("prefers tabs"))
        .stdout(predicate::str::contains("[global]"));
}

#[test]
fn show_prints_the_body() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_global(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["memory", "show", "pref"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Use tabs, not spaces."));
}

#[test]
fn list_empty_is_friendly() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no memories stored"));
}

#[test]
fn show_unknown_errors() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["memory", "show", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no memory named"));
}
