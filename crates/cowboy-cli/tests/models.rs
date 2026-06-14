//! CLI tests for `cowboy models` (list / use), with the home config dir
//! isolated via `XDG_CONFIG_HOME` so they never touch the developer's real
//! `~/.config/cowboy`.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

/// Seed an isolated home with a provider + two models (default = sonnet).
fn seed_home(home: &assert_fs::TempDir) {
    home.child("cowboy/providers.yaml")
        .write_str(
            "version: 1\nproviders:\n  litellm:\n    base_url: https://gw.local/v1\n    api_key: sk-secret-xyz\n",
        )
        .unwrap();
    home.child("cowboy/models.yaml")
        .write_str(
            "version: 1\ndefault: sonnet\nmodels:\n  sonnet:\n    provider: litellm\n    model: anthropic/claude-sonnet-4-6\n  cheap:\n    provider: litellm\n    model: openai/gpt-5.4-mini\n",
        )
        .unwrap();
}

#[test]
fn list_shows_providers_and_models_without_leaking_the_key() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("litellm"))
        .stdout(predicate::str::contains("https://gw.local/v1"))
        .stdout(predicate::str::contains("key: set"))
        .stdout(predicate::str::contains("sonnet"))
        .stdout(predicate::str::contains("default: sonnet"))
        // The actual key value must never be printed.
        .stdout(predicate::str::contains("sk-secret-xyz").not());
}

#[test]
fn use_global_sets_user_default() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "use", "--global", "cheap"])
        .assert()
        .success()
        .stdout(predicate::str::contains("user default is now `cheap`"));

    home.child("cowboy/models.yaml")
        .assert(predicate::str::contains("default: cheap"));
}

#[test]
fn use_writes_project_default() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "use", "cheap"])
        .assert()
        .success()
        .stdout(predicate::str::contains("project default is now `cheap`"));

    // Project file is created with the override; no credentials in it.
    proj.child(".cowboy/models.yaml")
        .assert(predicate::str::contains("default: cheap"));
    proj.child(".cowboy/models.yaml")
        .assert(predicate::str::contains("api_key").not());
}

#[test]
fn use_unknown_model_errors() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "use", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown model"));
}
