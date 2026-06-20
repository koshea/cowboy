//! Tests for config loading, defaults, validation, and the security invariant.
// Tests build configs by tweaking `default()` — idiomatic here, not worth the
// struct-literal noise.
#![allow(clippy::field_reassign_with_default)]

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
}

#[test]
fn security_config_save_roundtrips() {
    let tmp = tempdir();
    let mut cfg = SecurityConfig::default();
    cfg.networks.compose.approved.push("myapp_default".into());
    let p = tmp.path().join("security.yaml");
    cfg.save(&p).unwrap();
    let reloaded = SecurityConfig::load(&p).unwrap();
    assert_eq!(reloaded.networks.compose.approved, vec!["myapp_default"]);
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
fn security_invariant_rejects_mounting_providers_or_home_config() {
    // providers.yaml holds host API keys and must never reach the agent, via a
    // mount or a credential grant — directly or by mounting the home config dir.
    for source in [
        "~/.config/cowboy/providers.yaml",
        "~/.config/cowboy", // home config dir (basename "cowboy")
    ] {
        let mut cfg = SecurityConfig::default();
        cfg.container.mounts.push(Mount {
            source: source.into(),
            target: "/tmp/x".into(),
            mode: "ro".into(),
        });
        assert!(
            matches!(cfg.validate(), Err(Error::SecurityInvariant(_))),
            "mount of {source:?} must be rejected"
        );

        let mut cfg = SecurityConfig::default();
        cfg.secrets.files.push(grant(source, "/tmp/x"));
        assert!(
            matches!(cfg.validate(), Err(Error::SecurityInvariant(_))),
            "grant of {source:?} must be rejected"
        );
    }
}

#[test]
fn security_invariant_rejects_mounting_an_ancestor_of_the_config_dir() {
    // Mounting a parent of the home cowboy config dir (e.g. `~` or `~/.config`)
    // would drag providers.yaml into the container — must be refused, not just the
    // exact dir/file (basename) like before.
    for source in ["~", "~/.config"] {
        let mut cfg = SecurityConfig::default();
        cfg.container.mounts.push(Mount {
            source: source.into(),
            target: "/tmp/x".into(),
            mode: "ro".into(),
        });
        assert!(
            matches!(cfg.validate(), Err(Error::SecurityInvariant(_))),
            "mount of ancestor {source:?} must be rejected"
        );
    }
}

#[test]
fn security_invariant_rejects_unknown_mount_mode() {
    // A typo'd mode must fail closed rather than silently become read-write.
    let mut cfg = SecurityConfig::default();
    cfg.container.mounts.push(Mount {
        source: "/tmp/data".into(),
        target: "/data".into(),
        mode: "readonly".into(),
    });
    assert!(matches!(cfg.validate(), Err(Error::SecurityInvariant(_))));
}

fn grant(source: &str, target: &str) -> SecretMount {
    SecretMount {
        source: source.into(),
        target: target.into(),
        read_only: true,
        required: false,
        approval: None,
    }
}

#[test]
fn approval_required_recognizes_opt_in_values() {
    assert!(approval_required(&Some("required".into())));
    assert!(approval_required(&Some("ask".into())));
    assert!(!approval_required(&None));
    assert!(!approval_required(&Some("no".into())));
    let mut m = grant("~/.config/gh", "/tmp/.config/gh");
    assert!(!m.needs_approval());
    m.approval = Some("required".into());
    assert!(m.needs_approval());
}

#[test]
fn credential_grant_to_a_normal_path_validates() {
    let mut cfg = SecurityConfig::default();
    cfg.secrets
        .files
        .push(grant("~/.config/gh", "/tmp/.config/gh"));
    assert!(cfg.validate().is_ok());
}

#[test]
fn credential_grant_cannot_expose_host_config() {
    let mut cfg = SecurityConfig::default();
    cfg.secrets
        .files
        .push(grant(".cowboy/security.yaml", "/tmp/x"));
    assert!(matches!(cfg.validate(), Err(Error::SecurityInvariant(_))));
}

#[test]
fn credential_grant_cannot_shadow_the_workspace() {
    let mut cfg = SecurityConfig::default();
    // Target inside the workdir would shadow the project / masked config.
    cfg.secrets
        .files
        .push(grant("~/.config/gh", "/workspace/.config/gh"));
    assert!(matches!(cfg.validate(), Err(Error::SecurityInvariant(_))));
    // ...and a relative target is rejected too.
    let mut cfg = SecurityConfig::default();
    cfg.secrets.files.push(grant("~/.config/gh", "tmp/gh"));
    assert!(matches!(cfg.validate(), Err(Error::SecurityInvariant(_))));
}

#[test]
fn expand_path_resolves_tilde_and_vars() {
    std::env::set_var("COWBOY_TEST_CRED_DIR", "/secrets/dir");
    let p = expand_path("${COWBOY_TEST_CRED_DIR}/gh").unwrap();
    assert_eq!(p, std::path::PathBuf::from("/secrets/dir/gh"));
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    assert_eq!(
        expand_path("~/.config/gh").unwrap(),
        home.join(".config/gh")
    );
    assert_eq!(
        expand_path("/abs/path").unwrap(),
        std::path::PathBuf::from("/abs/path")
    );
}

#[test]
fn warnings_flag_dangerous_options() {
    let mut cfg = SecurityConfig::default();
    assert!(cfg.warnings().is_empty());
    cfg.container.privileged = true;
    cfg.container.docker_socket = true;
    assert_eq!(cfg.warnings().len(), 2);
}

fn provider(base_url: &str) -> Provider {
    Provider {
        base_url: base_url.into(),
        api_key: "sk-test".into(),
        headers: BTreeMap::new(),
    }
}

fn model_def(provider: &str, model: &str) -> ModelDef {
    ModelDef {
        provider: provider.into(),
        model: model.into(),
        temperature: 0.2,
        max_tokens: 8192,
        context_window: 200_000,
        reasoning_effort: None,
        top_p: None,
        stop: Vec::new(),
        extra: std::collections::BTreeMap::new(),
        headers: BTreeMap::new(),
        input_cost_per_mtok: None,
        output_cost_per_mtok: None,
        anthropic_cache: false,
    }
}

#[test]
fn resolve_model_merges_and_picks_default() {
    let mut providers = ProvidersConfig::default();
    providers
        .providers
        .insert("p".into(), provider("https://api/v1"));

    let mut user = ModelsConfig::default();
    user.default = Some("sonnet".into());
    user.models
        .insert("sonnet".into(), model_def("p", "anthropic/sonnet"));
    user.models
        .insert("cheap".into(), model_def("p", "openai/mini"));

    let mut project = ModelsConfig::default();
    // Project overrides "cheap" by name and adds a new model + a new default.
    project.default = Some("cheap".into());
    project
        .models
        .insert("cheap".into(), model_def("p", "project/override"));

    // Default precedence: project default ("cheap") wins over user default.
    let r = resolve_model(&providers, Some(&user), Some(&project), None).unwrap();
    assert_eq!(r.model, "project/override");
    assert_eq!(r.base_url, "https://api/v1");
    assert_eq!(r.api_key, "sk-test");

    // Explicit name beats both defaults; user model still reachable.
    let r = resolve_model(&providers, Some(&user), Some(&project), Some("sonnet")).unwrap();
    assert_eq!(r.model, "anthropic/sonnet");
}

#[test]
fn resolve_model_errors_clearly() {
    let providers = ProvidersConfig::default();
    // No models at all.
    assert!(resolve_model(&providers, None, None, None).is_err());

    // Model references an unknown provider.
    let mut user = ModelsConfig::default();
    user.default = Some("m".into());
    user.models.insert("m".into(), model_def("ghost", "x"));
    let err = resolve_model(&providers, Some(&user), None, None).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)));

    // Models exist but no default is set anywhere.
    let mut providers = ProvidersConfig::default();
    providers
        .providers
        .insert("p".into(), provider("https://api/v1"));
    let mut nodefault = ModelsConfig::default();
    nodefault.models.insert("m".into(), model_def("p", "x"));
    assert!(resolve_model(&providers, Some(&nodefault), None, None).is_err());
}

#[test]
fn project_models_reject_credentials() {
    // deny_unknown_fields makes a stray api_key/base_url a hard parse error,
    // so provider credentials can never hide in a project models.yaml.
    let tmp = tempdir();
    let p = write(
        tmp.path(),
        MODELS_FILE,
        "version: 1\nmodels:\n  m:\n    provider: p\n    model: x\n    api_key: sk-leak\n",
    );
    assert!(ModelsConfig::load(&p).is_err());
}

#[test]
fn providers_save_is_owner_only() {
    let tmp = tempdir();
    let path = tmp.path().join("providers.yaml");
    let mut cfg = ProvidersConfig::default();
    cfg.providers.insert("p".into(), provider("https://api/v1"));
    cfg.save(&path).unwrap();
    // Round-trips from the same path.
    let reloaded = ProvidersConfig::load(&path).unwrap();
    assert_eq!(reloaded, cfg);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "providers.yaml must be 0600");
    }
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

#[test]
fn cpus_accepts_number_or_auto() {
    // A bare number.
    let c: ContainerConfig = serde_yaml_ng::from_str("image: x\ncpus: 2\n").unwrap();
    assert_eq!(c.cpus, Some(CpuLimit::Cores(2.0)));
    // The `auto` keyword (case-insensitive).
    let c: ContainerConfig = serde_yaml_ng::from_str("image: x\ncpus: auto\n").unwrap();
    assert_eq!(c.cpus, Some(CpuLimit::Auto));
    // Absent → None (unlimited).
    let c: ContainerConfig = serde_yaml_ng::from_str("image: x\n").unwrap();
    assert_eq!(c.cpus, None);
    // Garbage is rejected.
    assert!(serde_yaml_ng::from_str::<ContainerConfig>("image: x\ncpus: lots\n").is_err());
    // Round-trips.
    let yaml = serde_yaml_ng::to_string(&CpuLimit::Auto).unwrap();
    assert_eq!(yaml.trim(), "auto");
}

#[test]
fn auto_resource_clamps() {
    // cpus = half the cores, clamped to [2, 8].
    assert_eq!(auto_cpus(1), 2.0);
    assert_eq!(auto_cpus(4), 2.0);
    assert_eq!(auto_cpus(32), 8.0);
    assert_eq!(auto_cpus(12), 6.0);
    // memory = quarter of RAM, clamped to [4 GiB, 16 GiB] (MiB).
    assert_eq!(auto_mem_mib(8 * 1024), 4096); // 8 GiB host → 2 GiB → clamp 4 GiB
    assert_eq!(auto_mem_mib(64 * 1024), 16384); // 64 GiB → 16 GiB → clamp 16 GiB
    assert_eq!(auto_mem_mib(32 * 1024), 8192); // 32 GiB → 8 GiB
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
fn expand_env_interpolates_and_fails_loudly() {
    // Literal text and bare `$` pass through unchanged.
    assert_eq!(
        expand_env("https://api.example/v1").unwrap(),
        "https://api.example/v1"
    );
    assert_eq!(expand_env("cost is $5").unwrap(), "cost is $5");

    // A unique var name keeps this isolated (nextest runs tests per-process).
    std::env::set_var("COWBOY_TEST_BASE_URL_XYZ", "https://gw.internal/v1");
    assert_eq!(
        expand_env("${COWBOY_TEST_BASE_URL_XYZ}/chat").unwrap(),
        "https://gw.internal/v1/chat"
    );

    // Unset / empty variable is an error, not a silent empty URL.
    std::env::remove_var("COWBOY_TEST_UNSET_VAR_XYZ");
    assert!(expand_env("${COWBOY_TEST_UNSET_VAR_XYZ}/v1").is_err());
    // Unterminated `${` is rejected.
    assert!(expand_env("${OOPS").is_err());
}

#[test]
fn provider_base_url_resolves_from_env() {
    // A provider may still use ${VAR} in its endpoint; resolve_model expands it.
    std::env::set_var("COWBOY_TEST_PROVIDER_URL", "https://gw.local/v1");
    let mut providers = ProvidersConfig::default();
    providers.providers.insert(
        "p".into(),
        Provider {
            base_url: "${COWBOY_TEST_PROVIDER_URL}".into(),
            api_key: "sk-test".into(),
            headers: BTreeMap::new(),
        },
    );
    let mut user = ModelsConfig::default();
    user.default = Some("m".into());
    user.models.insert("m".into(), model_def("p", "x"));

    let r = resolve_model(&providers, Some(&user), None, None).unwrap();
    assert_eq!(r.base_url, "https://gw.local/v1");
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
