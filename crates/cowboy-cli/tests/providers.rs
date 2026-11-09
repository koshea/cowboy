//! CLI tests for `cowboy providers` (list / add / remove), with the home config
//! dir isolated via `XDG_CONFIG_HOME` so they never touch the real
//! `~/.config/cowboy`.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy(home: &assert_fs::TempDir, proj: &assert_fs::TempDir) -> Command {
    let mut c = Command::cargo_bin("cowboy").unwrap();
    c.current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path());
    c
}

fn seed(home: &assert_fs::TempDir) {
    home.child("cowboy/providers.yaml")
        .write_str(
            "version: 1\nproviders:\n  litellm:\n    base_url: https://gw.local/v1\n    \
             api_key: sk-secret-xyz\n",
        )
        .unwrap();
    home.child("cowboy/models.yaml")
        .write_str(
            "version: 1\ndefault: litellm/sonnet\nmodels:\n  litellm:\n    sonnet:\n      \
             model: anthropic/claude-sonnet-4-6\n",
        )
        .unwrap();
}

#[test]
fn add_a_second_provider_beside_the_first() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);

    cowboy(&home, &proj)
        .args([
            "providers",
            "add",
            "bifrost",
            "--base-url",
            "http://localhost:8080/v1",
            "--api",
            "responses",
            "--no-key",
        ])
        .assert()
        .success();
    // A keyed one: the key is read from stdin when not on a terminal.
    cowboy(&home, &proj)
        .args([
            "providers",
            "add",
            "openai",
            "--base-url",
            "https://api.openai.com/v1",
        ])
        .write_stdin("sk-openai-abc\n")
        .assert()
        .success();

    cowboy(&home, &proj)
        .args(["providers", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("litellm"))
        .stdout(predicate::str::contains("bifrost"))
        .stdout(predicate::str::contains("api:      responses"))
        .stdout(predicate::str::contains("none (keyless)"))
        .stdout(predicate::str::contains("openai"))
        .stdout(predicate::str::contains("sk-openai-abc").not())
        .stdout(predicate::str::contains("sk-secret-xyz").not());

    // The original provider and its model are untouched.
    home.child("cowboy/providers.yaml")
        .assert(predicate::str::contains("sk-secret-xyz"))
        .assert(predicate::str::contains("api: responses"));
    home.child("cowboy/models.yaml")
        .assert(predicate::str::contains("default: litellm/sonnet"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path().join("cowboy/providers.yaml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn adding_an_existing_provider_is_refused() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    cowboy(&home, &proj)
        .args([
            "providers",
            "add",
            "litellm",
            "--base-url",
            "https://x/v1",
            "--no-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
    home.child("cowboy/providers.yaml")
        .assert(predicate::str::contains("https://gw.local/v1"));
}

#[test]
fn removing_a_provider_in_use_is_refused_without_force() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    cowboy(&home, &proj)
        .args(["providers", "remove", "litellm"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("litellm/sonnet"))
        .stderr(predicate::str::contains("--force"));
    home.child("cowboy/providers.yaml")
        .assert(predicate::str::contains("litellm"));
}

#[test]
fn removing_an_unused_provider_succeeds() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    cowboy(&home, &proj)
        .args([
            "providers",
            "add",
            "spare",
            "--base-url",
            "https://s/v1",
            "--no-key",
        ])
        .assert()
        .success();
    cowboy(&home, &proj)
        .args(["providers", "remove", "spare"])
        .assert()
        .success();
    home.child("cowboy/providers.yaml")
        .assert(predicate::str::contains("spare").not())
        .assert(predicate::str::contains("litellm"));
}

#[test]
fn forced_removal_prunes_its_home_models_and_default() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    cowboy(&home, &proj)
        .env("COWBOY_ASSUME_YES", "1")
        .args(["providers", "remove", "litellm", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("held your default model"));
    home.child("cowboy/models.yaml")
        .assert(predicate::str::contains("sonnet").not());
}

fn seed_chatgpt(home: &assert_fs::TempDir) {
    home.child("cowboy/providers.yaml")
        .write_str(
            "version: 1\nproviders:\n  chatgpt:\n    base_url: https://api.openai.com/v1\n    \
             api: responses\n    auth: chatgpt\n",
        )
        .unwrap();
    home.child("cowboy/crew.yaml")
        .write_str("version: 1\ncrews:\n  openai:\n    crew:\n      general: chatgpt/gpt\n")
        .unwrap();
    home.child("cowboy/models.yaml")
        .write_str("default: chatgpt/gpt\nmodels:\n  chatgpt:\n    gpt:\n      model: gpt-6\n")
        .unwrap();
}

#[test]
fn a_chatgpt_provider_reports_its_sign_in_state() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed_chatgpt(&home);
    cowboy(&home, &proj)
        .args(["providers", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not signed in"))
        .stdout(predicate::str::contains("cowboy providers login chatgpt"));
    // A roster routed there is flagged before any subagent is sent to it.
    cowboy(&home, &proj)
        .args(["crew", "validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not signed in"));

    // Signed in, with plan usage.
    home.child("cowboy/auth/chatgpt.yaml")
        .write_str(
            "client_id: oaiapp_1\nemail: u@example.com\naccess_token: a\nrefresh_token: r\n\
             expires_at_ms: 1\nscopes: [openid, chatgpt.tokens.use.direct]\n",
        )
        .unwrap();
    cowboy(&home, &proj)
        .args(["providers", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "signed in as u@example.com, plan usage enabled",
        ))
        .stdout(predicate::str::contains("a\n").not());
    cowboy(&home, &proj)
        .args(["crew", "validate"])
        .assert()
        .success();
}

#[test]
fn login_refuses_a_provider_that_is_not_chatgpt() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    cowboy(&home, &proj)
        .args(["providers", "login", "litellm"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not sign in with ChatGPT"));
}
