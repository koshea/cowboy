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
            "version: 1\ndefault: litellm/sonnet\nmodels:\n  litellm:\n    sonnet:\n      model: anthropic/claude-sonnet-4-6\n    cheap:\n      model: openai/gpt-5.4-mini\n",
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
        .stdout(predicate::str::contains("litellm/sonnet"))
        .stdout(predicate::str::contains("default: litellm/sonnet"))
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
        .args(["models", "use", "--global", "litellm/cheap"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "user default is now `litellm/cheap`",
        ));

    home.child("cowboy/models.yaml")
        .assert(predicate::str::contains("default: litellm/cheap"));
}

#[test]
fn use_writes_project_default() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "use", "litellm/cheap"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "project default is now `litellm/cheap`",
        ));

    // Project file is created with the override; no credentials in it.
    proj.child(".cowboy/models.yaml")
        .assert(predicate::str::contains("default: litellm/cheap"));
    proj.child(".cowboy/models.yaml")
        .assert(predicate::str::contains("api_key").not());
}

#[test]
fn add_registers_a_model_with_shipped_defaults() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    // A known model id: name + reasoning + context come from the shipped table.
    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "add", "cerebras/zai-glm-4.7"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Cerebras: GLM 4.7"));

    home.child("cowboy/models.yaml")
        .assert(predicate::str::contains("Cerebras: GLM 4.7"))
        .assert(predicate::str::contains("model: cerebras/zai-glm-4.7"))
        .assert(predicate::str::contains("reasoning_effort: high"))
        .assert(predicate::str::contains("context_window: 131072"));
}

#[test]
fn add_honors_overrides() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args([
            "models",
            "add",
            "cerebras/zai-glm-4.7",
            "--name",
            "litellm/glm",
            "--temp",
            "0.3",
            "--reasoning",
            "none",
            "--default",
        ])
        .assert()
        .success();

    home.child("cowboy/models.yaml")
        .assert(predicate::str::contains("    glm:"))
        .assert(predicate::str::contains("temperature: 0.3"))
        .assert(predicate::str::contains("default: litellm/glm"))
        // reasoning explicitly disabled -> field omitted
        .assert(predicate::str::contains("reasoning_effort").not());
}

#[test]
fn use_unknown_model_errors() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "use", "litellm/ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown model"));
}

#[test]
fn a_bare_model_name_is_refused_with_the_migrate_hint() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "use", "cheap"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cowboy models migrate"));
}

#[test]
fn add_names_the_model_under_its_provider() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();
    seed_home(&home);

    // An unknown provider in `--name` is refused rather than silently created.
    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "add", "x/y", "--name", "ghost/y"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown provider `ghost`"));

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["models", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("litellm/"))
        .stdout(predicate::str::contains("litellm/cheap"));
}
