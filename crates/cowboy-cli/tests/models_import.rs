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

/// A ChatGPT provider with a signed-in record and the catalogue shape recorded
/// from a real account.
async fn chatgpt_home() -> (MockServer, assert_fs::TempDir) {
    let server = MockServer::start().await;
    let m = |slug: &str, w: u64| {
        json!({"slug": slug, "display_name": slug, "visibility": "list",
        "context_window": w, "default_reasoning_level": "medium",
        "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}]})
    };
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [
            m("gpt-6.1-sol", 500000), m("gpt-6-astra", 272000), m("gpt-6.1-terra", 500000),
            m("gpt-6-sol", 272000), m("gpt-6-luna", 272000), m("gpt-5.6-luna", 272000),
        ]})))
        .mount(&server)
        .await;
    let home = assert_fs::TempDir::new().unwrap();
    home.child("cowboy/providers.yaml")
        .write_str(&format!(
            "version: 1\nproviders:\n  chatgpt:\n    base_url: {}/v1\n    api: responses\n    \
             auth: chatgpt\n",
            server.uri()
        ))
        .unwrap();
    let far = cowboy_core::time::now_ms() + 3_600_000;
    home.child("cowboy/auth/chatgpt.yaml")
        .write_str(&format!(
            "client_id: oaiapp_1\naccess_token: tok\nrefresh_token: r\nexpires_at_ms: {far}\n\
             saved_at_ms: {far}\nscopes: [chatgpt.tokens.use.direct]\n"
        ))
        .unwrap();
    (server, home)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chatgpt_preset_imports_its_models_and_adds_a_valid_crew() {
    let (_server, home) = chatgpt_home().await;
    cowboy(&home)
        .args(["crew", "new", "openai", "--preset", "chatgpt"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "luna chatgpt/gpt-6-luna, terra chatgpt/gpt-6.1-terra, sol chatgpt/gpt-6.1-sol (foreman), astra chatgpt/gpt-6-astra",
        ));
    let m = models(&home);
    assert_eq!(m.models["chatgpt"].len(), 4, "only the four the crew uses");
    let sol = m
        .get(&cowboy_core::config::ModelRef::parse("chatgpt/gpt-6.1-sol").unwrap())
        .unwrap();
    assert_eq!(sol.context_window, 500_000);
    let f = cowboy_core::crew::load_file_at(&home.path().join("cowboy/crew.yaml"))
        .unwrap()
        .unwrap();
    assert_eq!(
        f.active.as_deref(),
        Some("openai"),
        "the first crew is active"
    );
    assert_eq!(
        f.crews["openai"].foreman.as_ref().unwrap().to_string(),
        "chatgpt/gpt-6.1-sol@medium"
    );
    assert_eq!(
        sol.reasoning_levels.len(),
        3,
        "the accepted levels are recorded"
    );
    let debugging = &f.crews["openai"].crew["debugging"];
    use cowboy_core::crew::Effort;
    assert_eq!(
        debugging.pick(Effort::Large),
        Some("chatgpt/gpt-6.1-sol@high")
    );
    // This catalogue lists no `xhigh`, so the slot is left at astra's own level.
    assert_eq!(debugging.pick(Effort::Deep), Some("chatgpt/gpt-6-astra"));
    cowboy(&home).args(["crew", "validate"]).assert().success();
    // A second preset crew reuses the configured models instead of importing again.
    cowboy(&home)
        .args(["crew", "new", "openai2", "--preset", "chatgpt"])
        .assert()
        .success()
        .stdout(predicate::str::contains("importing").not());
}
