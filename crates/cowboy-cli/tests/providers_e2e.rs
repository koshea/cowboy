//! **Manually-run** end-to-end tests for the wire APIs and providers: a real
//! tool-using turn over a Responses endpoint (e.g. a Bifrost gateway), over
//! ChatGPT plan usage, and a crew that routes to two providers at once.
//!
//! Each needs the sandbox and a real, configured provider in `~/.config/cowboy`,
//! and **self-skips** (passes, printing why) when that is absent, so `--ignored`
//! is safe anywhere:
//!
//! ```sh
//! cargo test -p cowboy-cli --test providers_e2e -- --ignored --nocapture
//! ```
//!
//! The user's config is copied into a throwaway config home first (ChatGPT sign-in
//! records included — the copy is 0600 and deleted with the temp dir), so a test
//! can never rewrite the real files.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_fs::prelude::*;
use cowboy_core::config::{
    merged_models, ModelRef, ModelsConfig, ProviderAuth, ProvidersConfig, WireApi,
};

fn real_config_dir() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        roots.push(PathBuf::from(x));
    }
    if let Some(h) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        roots.push(PathBuf::from(h).join(".config"));
    }
    roots
        .into_iter()
        .map(|r| r.join("cowboy"))
        .find(|d| d.join("providers.yaml").is_file())
}

/// Whether this host can create the sandbox's user namespace — the prerequisite
/// everything else rests on (`cowboy doctor` reports the full list).
fn sandbox_ok() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "--", "/bin/true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The real config, loaded. `None` (with a reason) when it is missing or still
/// in the pre-`provider/name` layout.
fn real_config() -> Result<(PathBuf, ProvidersConfig, ModelsConfig), String> {
    let dir = real_config_dir().ok_or("no ~/.config/cowboy/providers.yaml")?;
    let providers =
        ProvidersConfig::load(&dir.join("providers.yaml")).map_err(|e| e.to_string())?;
    let models = ModelsConfig::load(&dir.join("models.yaml")).map_err(|e| e.to_string())?;
    Ok((dir, providers, models))
}

/// The first configured model whose provider matches `want`.
fn model_where(
    providers: &ProvidersConfig,
    models: &ModelsConfig,
    want: impl Fn(&str, &cowboy_core::config::Provider) -> bool,
) -> Option<ModelRef> {
    // `COWBOY_E2E_MODEL` picks among the candidates; it never widens them, so a
    // test cannot pass on a model that is not the kind it is about.
    let pick = std::env::var("COWBOY_E2E_MODEL").ok();
    merged_models(Some(models), None).into_keys().find(|r| {
        pick.as_deref().is_none_or(|m| m == r.to_string())
            && providers
                .providers
                .get(&r.provider)
                .is_some_and(|p| want(&r.provider, p))
    })
}

/// A throwaway config home holding a copy of the real one.
fn copy_config(real: &Path) -> assert_fs::TempDir {
    let home = assert_fs::TempDir::new().unwrap();
    let dst = home.child("cowboy");
    dst.create_dir_all().unwrap();
    for name in ["providers.yaml", "models.yaml", "harnesses.yaml"] {
        if real.join(name).is_file() {
            std::fs::copy(real.join(name), dst.path().join(name)).unwrap();
        }
    }
    if real.join("auth").is_dir() {
        let auth = dst.child("auth");
        auth.create_dir_all().unwrap();
        for e in std::fs::read_dir(real.join("auth")).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "yaml") || e.file_name() == "host_id" {
                std::fs::copy(e.path(), auth.path().join(e.file_name())).unwrap();
            }
        }
    }
    home
}

fn make_project() -> assert_fs::TempDir {
    let dir = assert_fs::TempDir::new().unwrap();
    let _ = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q"])
        .status();
    dir.child("README.md").write_str("hello\n").unwrap();
    dir.child("src/a.rs").write_str("fn a() {}\n").unwrap();
    dir.child("src/b.rs").write_str("fn b() {}\n").unwrap();
    let ok = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(dir.path())
        .arg("init")
        .output()
        .expect("run cowboy init")
        .status
        .success();
    assert!(ok, "cowboy init should succeed");
    dir
}

/// One headless turn on `model`; returns (success, stdout, stderr).
fn run_turn(home: &Path, project: &Path, model: &ModelRef, task: &str) -> (bool, String, String) {
    let state = assert_fs::TempDir::new().unwrap();
    let runtime = assert_fs::TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cowboy"))
        .current_dir(project)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("COWBOY_MODEL", model.to_string())
        .arg("--")
        .arg(task)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run cowboy");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The tool calls the session's transcript recorded.
fn tool_calls(project: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let sessions = project.join(".cowboy/sessions");
    for e in std::fs::read_dir(sessions).into_iter().flatten().flatten() {
        let Ok(text) = std::fs::read_to_string(e.path().join("transcript.jsonl")) else {
            continue;
        };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let m = v.get("message").unwrap_or(&v);
            for c in m["tool_calls"].as_array().into_iter().flatten() {
                out.push(c["name"].as_str().unwrap_or_default().to_string());
            }
        }
    }
    out
}

const TASK: &str = "Use the shell tool to run exactly: ls src. Then reply with only the \
                    number of entries it printed, as a digit.";

fn tool_turn(model: &ModelRef, real: &Path) {
    let home = copy_config(real);
    let project = make_project();
    let (ok, stdout, stderr) = run_turn(home.path(), project.path(), model, TASK);
    assert!(ok, "the turn on {model} failed:\n{stderr}");
    assert!(
        stdout.contains('2'),
        "expected the entry count (2) from {model}, got:\n{stdout}"
    );
    let calls = tool_calls(project.path());
    assert!(
        calls.iter().any(|c| c == "shell"),
        "{model} answered without calling the shell tool: {calls:?}"
    );
}

#[test]
#[ignore = "real sandbox + model: a tool turn over an `api: responses` provider"]
fn e2e_a_tool_turn_over_the_responses_api() {
    if !sandbox_ok() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
        return;
    }
    let (dir, providers, models) = match real_config() {
        Ok(c) => c,
        Err(why) => return eprintln!("skipping: {why}"),
    };
    let Some(model) = model_where(&providers, &models, |_, p| {
        p.api == WireApi::Responses && p.auth == ProviderAuth::ApiKey
    }) else {
        return eprintln!("skipping: no model on an `api: responses` API-key provider");
    };
    tool_turn(&model, &dir);
}

#[test]
#[ignore = "real sandbox + model: a tool turn on ChatGPT plan usage"]
fn e2e_a_tool_turn_on_chatgpt_plan_usage() {
    if !sandbox_ok() {
        eprintln!("skipping: the sandbox cannot run here (see `cowboy doctor`)");
        return;
    }
    let (dir, providers, models) = match real_config() {
        Ok(c) => c,
        Err(why) => return eprintln!("skipping: {why}"),
    };
    let signed_in = |name: &str| {
        cowboy_core::oauth::chatgpt::load(&dir.join("auth"), name)
            .ok()
            .flatten()
            .is_some_and(|r| r.signed_in() && r.plan_usage_enabled())
    };
    let Some(model) = model_where(&providers, &models, |name, p| {
        p.auth == ProviderAuth::Chatgpt && signed_in(name)
    }) else {
        return eprintln!(
            "skipping: no model on a signed-in ChatGPT provider \
             (`cowboy providers add chatgpt --chatgpt`)"
        );
    };
    tool_turn(&model, &dir);
}
