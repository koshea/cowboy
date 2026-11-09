//! `cowboy models import` / `refresh` against a mock provider catalogue, with the
//! home config isolated via `XDG_CONFIG_HOME`.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn gateway() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "fireworks/accounts/fireworks/models/glm-5p3", "context_window": 1048576},
            {"id": "cerebras/zai-glm-4.7", "context_window": 131072},
            {"id": "anthropic/claude-sonnet-4-6", "context_window": 1000000},
            {"id": "openai/text-embedding-3-large"},
        ]})))
        .mount(&server)
        .await;
    server
}

fn home(server: &MockServer) -> assert_fs::TempDir {
    let home = assert_fs::TempDir::new().unwrap();
    home.child("cowboy/providers.yaml")
        .write_str(&format!(
            "version: 1\nproviders:\n  bifrost:\n    base_url: {}/v1\n",
            server.uri()
        ))
        .unwrap();
    home
}

fn cowboy(home: &assert_fs::TempDir) -> Command {
    let proj = std::env::temp_dir();
    let mut c = Command::cargo_bin("cowboy").unwrap();
    c.current_dir(proj).env("XDG_CONFIG_HOME", home.path());
    c
}

fn models(home: &assert_fs::TempDir) -> cowboy_core::config::ModelsConfig {
    cowboy_core::config::ModelsConfig::load(&home.path().join("cowboy/models.yaml")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn import_saves_chat_models_with_catalogue_windows() {
    let server = gateway().await;
    let home = home(&server);
    cowboy(&home)
        .args(["models", "import", "bifrost", "--match", "glm", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("imported 2 model(s)"));
    let m = models(&home);
    let glm = m
        .get(&cowboy_core::config::ModelRef::parse("bifrost/glm-5p3").unwrap())
        .unwrap();
    assert_eq!(glm.context_window, 1_048_576);
    assert!(m.default.is_some(), "the first import sets a default");

    // A second import only offers what is new, and never the embedding model.
    cowboy(&home)
        .args(["models", "import", "bifrost", "--all", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("imported 1 model(s)"));
    let m = models(&home);
    assert_eq!(m.models["bifrost"].len(), 3);
    assert!(!m.models["bifrost"]
        .values()
        .any(|d| d.model.contains("embedding")));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_terminal_import_needs_a_choice() {
    let server = gateway().await;
    let home = home(&server);
    cowboy(&home)
        .args(["models", "import", "bifrost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--all or --match"));
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_corrects_windows_and_keeps_user_settings() {
    let server = gateway().await;
    let home = home(&server);
    home.child("cowboy/models.yaml")
        .write_str(
            "version: 1\ndefault: bifrost/glm\nmodels:\n  bifrost:\n    glm:\n      \
             model: cerebras/zai-glm-4.7\n      context_window: 8192\n      max_tokens: 2048\n      \
             temperature: 0.1\n",
        )
        .unwrap();
    cowboy(&home)
        .args(["models", "refresh", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("context_window    8192 → 131072"));
    let m = models(&home);
    let glm = m
        .get(&cowboy_core::config::ModelRef::parse("bifrost/glm").unwrap())
        .unwrap();
    assert_eq!(
        (glm.context_window, glm.max_tokens, glm.temperature),
        (131_072, 2048, 0.1)
    );
    assert!(home.path().join("cowboy/models.yaml.bak").exists());
    cowboy(&home)
        .args(["models", "refresh", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("up to date"));
}
