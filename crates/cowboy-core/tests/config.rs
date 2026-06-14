//! Tests for config loading, defaults, validation, and the security invariant.

use std::collections::BTreeMap;

use cowboy_core::config::*;
use cowboy_core::Error;

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn templates_parse_and_validate() {
    let tmp = tempdir();
    let sec = write(tmp.path(), SECURITY_FILE, &security_template());
    let agent = write(tmp.path(), AGENT_FILE, &agent_template());
    let models = write(tmp.path(), MODELS_FILE, &models_template());

    let s = SecurityConfig::load(&sec).expect("security loads");
    assert_eq!(s.version, 1);
    assert_eq!(s.network_policy.default_external, DefaultVerdict::Ask);
    // Default deny list must include the cloud metadata endpoint.
    assert!(s
        .network_policy
        .deny
        .cidrs
        .iter()
        .any(|c| c.starts_with("169.254.169.254")));

    let a = AgentConfig::load(&agent).expect("agent loads");
    assert_eq!(a.agent.command_timeout_seconds, 600);

    let m = ModelsConfig::load(&models).expect("models loads");
    let profile = m.resolve(None).expect("default profile resolves");
    assert_eq!(profile.api_key_env, "COWBOY_OPENAI_API_KEY");
    // The key itself must never appear in the file.
    assert!(!models_template().contains("sk-"));
}

#[test]
fn security_invariant_rejects_mounting_security_file() {
    let mut cfg = SecurityConfig::default();
    cfg.container.mounts.push(Mount {
        source: ".cowboy/security.yaml".into(),
        target: "/workspace/.cowboy/security.yaml".into(),
        mode: "ro".into(),
    });
    let err = cfg.validate().expect_err("must reject");
    assert!(matches!(err, Error::SecurityInvariant(_)), "got {err:?}");
}

#[test]
fn security_invariant_rejects_mounting_cowboy_dir() {
    let mut cfg = SecurityConfig::default();
    cfg.container.mounts.push(Mount {
        source: ".cowboy".into(),
        target: "/workspace/.cowboy".into(),
        mode: "rw".into(),
    });
    assert!(matches!(cfg.validate(), Err(Error::SecurityInvariant(_))));
}

#[test]
fn warnings_flag_dangerous_options() {
    let mut cfg = SecurityConfig::default();
    assert!(cfg.warnings().is_empty());
    cfg.container.privileged = true;
    cfg.container.docker_socket = true;
    assert_eq!(cfg.warnings().len(), 2);
}

#[test]
fn models_validate_rejects_missing_default() {
    let cfg = ModelsConfig {
        version: 1,
        models: ModelSet {
            default: "ghost".into(),
            profiles: BTreeMap::new(),
        },
    };
    assert!(matches!(cfg.validate(), Err(Error::Invalid(_))));
}

#[test]
fn missing_file_is_distinguishable() {
    let tmp = tempdir();
    let missing = tmp.path().join("nope.yaml");
    assert!(matches!(
        SecurityConfig::load(&missing),
        Err(Error::ConfigNotFound(_))
    ));
}

#[test]
fn partial_agent_yaml_uses_defaults() {
    let tmp = tempdir();
    let p = write(tmp.path(), AGENT_FILE, "version: 1\n");
    let a = AgentConfig::load(&p).unwrap();
    assert_eq!(a.agent.max_command_output_bytes, 60_000);
    assert!(a.processes.is_empty());
}

// Snapshot the rendered templates so changes are reviewed deliberately.
#[test]
fn snapshot_security_template() {
    insta::assert_snapshot!(security_template());
}
#[test]
fn snapshot_agent_template() {
    insta::assert_snapshot!(agent_template());
}
#[test]
fn snapshot_models_template() {
    insta::assert_snapshot!(models_template());
}

// --- tiny tempdir helper (avoids an extra dev-dependency) ---
struct TempDir(std::path::PathBuf);
impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tempdir() -> TempDir {
    let base = std::env::temp_dir();
    let unique = format!(
        "cowboy-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let p = base.join(unique);
    std::fs::create_dir_all(&p).unwrap();
    TempDir(p)
}
static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
