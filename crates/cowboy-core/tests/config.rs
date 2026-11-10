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
    cfg.sandbox.mounts.push(Mount {
        source: "/data".into(),
        target: "/data".into(),
        mode: "ro".into(),
    });
    cfg.sandbox.memory = Some("4g".into());
    let p = tmp.path().join("security.yaml");
    cfg.save(&p).unwrap();
    let reloaded = SecurityConfig::load(&p).unwrap();
    assert_eq!(reloaded.sandbox.mounts, cfg.sandbox.mounts);
    assert_eq!(reloaded.sandbox.memory.as_deref(), Some("4g"));
}

/// A config still using the pre-sandbox `container:` key is refused by name.
///
/// Silently ignoring the unknown section is the dangerous outcome, not the annoying
/// one: every mount under it would vanish, leaving the agent with only the default
/// workspace mount and no indication why the paths it had yesterday are gone.
#[test]
fn the_old_container_section_is_refused_with_a_clear_message() {
    let dir = tempdir();
    let p = dir.0.join("security.yaml");
    std::fs::write(
        &p,
        "version: 1\ncontainer:\n  workdir: /workspace\n  mounts:\n    - source: /data\n      \
         target: /data\n      mode: rw\n",
    )
    .unwrap();

    let err = SecurityConfig::load(&p).expect_err("the old key must be refused");
    let msg = err.to_string();
    assert!(matches!(err, Error::SecurityInvariant(_)), "got {err:?}");
    assert!(msg.contains("container:"), "name the old key: {msg}");
    assert!(msg.contains("sandbox:"), "and the new one: {msg}");
}

/// A mistyped key in `security.yaml` must fail the load, not fall back to defaults.
///
/// This is the failure mode the `container:` check above handles by name, generalised:
/// serde's default is to ignore what it does not recognise, so `netwrok_policy:` used
/// to leave the policy at its defaults and `deny:` rules the user wrote would simply
/// not exist. The defaults are fail-closed (`ask`), so this was never an open door —
/// but a security file that half-applies without saying so is exactly what host-owned
/// config is supposed to rule out.
#[test]
fn a_mistyped_security_key_fails_the_load() {
    let dir = tempdir();
    for (yaml, typo) in [
        (
            "version: 1\nnetwrok_policy:\n  default_external: deny\n",
            "netwrok_policy",
        ),
        (
            "version: 1\nnetwork_policy:\n  deny:\n    domians:\n      - evil.example\n",
            "domians",
        ),
        (
            "version: 1\nsandbox:\n  mounts:\n    - source: /data\n      target: /data\n      \
             readonly: true\n",
            "readonly",
        ),
        (
            "version: 1\nsecrets:\n  env:\n    - name: TOKEN\n      source: HOST_TOKEN\n",
            "source",
        ),
    ] {
        let p = dir.0.join("security.yaml");
        std::fs::write(&p, yaml).unwrap();
        let err = SecurityConfig::load(&p)
            .expect_err("a key cowboy does not understand must not be silently dropped");
        assert!(
            err.to_string().contains(typo),
            "the error must name the offending key {typo}: {err}"
        );
    }
}

/// The new key loads, and its mounts survive — the other half of the check above.
#[test]
fn the_sandbox_section_loads_its_mounts() {
    let dir = tempdir();
    let p = dir.0.join("security.yaml");
    std::fs::write(
        &p,
        "version: 1\nsandbox:\n  workdir: /workspace\n  mounts:\n    - source: /data\n      \
         target: /data\n      mode: rw\n",
    )
    .unwrap();

    let cfg = SecurityConfig::load(&p).expect("the new key must load");
    assert_eq!(cfg.sandbox.mounts.len(), 1);
    assert_eq!(cfg.sandbox.mounts[0].source, "/data");
}

#[test]
fn security_invariant_rejects_mounting_security_file() {
    let mut cfg = SecurityConfig::default();
    cfg.sandbox.mounts.push(Mount {
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
    cfg.sandbox.mounts.push(Mount {
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
        cfg.sandbox.mounts.push(Mount {
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
        cfg.sandbox.mounts.push(Mount {
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
    cfg.sandbox.mounts.push(Mount {
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

/// A broad read-write mount is permitted — someone may mean it — but must be
/// surfaced, because it hands the agent most of the machine and almost nobody means
/// it. This is the successor to the old `privileged`/`docker_socket` warnings.
#[test]
fn warnings_flag_a_mount_that_exposes_most_of_the_machine() {
    let cfg = SecurityConfig::default();
    assert!(
        cfg.warnings().is_empty(),
        "the default project-only mount is not worth warning about"
    );

    for broad in ["~", "/"] {
        let mut cfg = SecurityConfig::default();
        cfg.sandbox.mounts.push(Mount {
            source: broad.into(),
            target: "/host".into(),
            mode: "rw".into(),
        });
        let warnings = cfg.warnings();
        assert_eq!(warnings.len(), 1, "{broad} should warn: {warnings:?}");
        assert!(
            warnings[0].contains("cowboy grant"),
            "and point at the narrower alternative: {warnings:?}"
        );
    }
}

fn provider(base_url: &str) -> Provider {
    Provider::api_key(base_url, "sk-test")
}

fn mref(s: &str) -> ModelRef {
    ModelRef::parse(s).unwrap()
}

fn model_def(model: &str) -> ModelDef {
    ModelDef {
        model: model.into(),
        temperature: 0.2,
        max_tokens: 8192,
        context_window: 200_000,
        reasoning_effort: None,
        reasoning_levels: Vec::new(),
        top_p: None,
        stop: Vec::new(),
        extra: std::collections::BTreeMap::new(),
        headers: BTreeMap::new(),
        input_cost_per_mtok: None,
        output_cost_per_mtok: None,
        cached_input_cost_per_mtok: None,
        anthropic_cache: false,
        stream_idle_timeout_seconds: None,
    }
}

#[test]
fn resolve_model_merges_and_picks_default() {
    let mut providers = ProvidersConfig::default();
    providers
        .providers
        .insert("p".into(), provider("https://api/v1"));

    let mut user = ModelsConfig::default();
    user.default = Some(mref("p/sonnet"));
    user.insert(mref("p/sonnet"), model_def("anthropic/sonnet"));
    user.insert(mref("p/cheap"), model_def("openai/mini"));

    let mut project = ModelsConfig::default();
    // Project overrides "cheap" by name and adds a new model + a new default.
    project.default = Some(mref("p/cheap"));
    project.insert(mref("p/cheap"), model_def("project/override"));

    // Default precedence: project default ("cheap") wins over user default.
    let r = resolve_model(&providers, Some(&user), Some(&project), None).unwrap();
    assert_eq!(r.model, "project/override");
    assert_eq!(r.base_url, "https://api/v1");
    assert_eq!(r.auth, ResolvedAuth::ApiKey("sk-test".into()));
    assert_eq!(r.provider_name, "p");
    assert_eq!(r.api, WireApi::ChatCompletions);

    // Explicit name beats both defaults; user model still reachable.
    let r = resolve_model(
        &providers,
        Some(&user),
        Some(&project),
        Some(&mref("p/sonnet")),
    )
    .unwrap();
    assert_eq!(r.model, "anthropic/sonnet");
}

#[test]
fn resolve_model_errors_clearly() {
    let providers = ProvidersConfig::default();
    // No models at all.
    assert!(resolve_model(&providers, None, None, None).is_err());

    // Model references an unknown provider.
    let mut user = ModelsConfig::default();
    user.default = Some(mref("ghost/m"));
    user.insert(mref("ghost/m"), model_def("x"));
    let err = resolve_model(&providers, Some(&user), None, None).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)));

    // Models exist but no default is set anywhere.
    let mut providers = ProvidersConfig::default();
    providers
        .providers
        .insert("p".into(), provider("https://api/v1"));
    let mut nodefault = ModelsConfig::default();
    nodefault.insert(mref("p/m"), model_def("x"));
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
        "version: 1\nmodels:\n  p:\n    m:\n      model: x\n      api_key: sk-leak\n",
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
    let c: SandboxConfig = serde_yaml_ng::from_str("cpus: 2\n").unwrap();
    assert_eq!(c.cpus, Some(CpuLimit::Cores(2.0)));
    // The `auto` keyword (case-insensitive).
    let c: SandboxConfig = serde_yaml_ng::from_str("cpus: auto\n").unwrap();
    assert_eq!(c.cpus, Some(CpuLimit::Auto));
    // Absent → None (unlimited).
    let c: SandboxConfig = serde_yaml_ng::from_str("{}\n").unwrap();
    assert_eq!(c.cpus, None);
    // Garbage is rejected.
    assert!(serde_yaml_ng::from_str::<SandboxConfig>("cpus: lots\n").is_err());
    // Nonsensical numbers are rejected: zero, negative, and non-finite would each
    // corrupt the cgroup quota / build parallelism.
    assert!(serde_yaml_ng::from_str::<SandboxConfig>("cpus: 0\n").is_err());
    assert!(serde_yaml_ng::from_str::<SandboxConfig>("cpus: -4\n").is_err());
    assert!(serde_yaml_ng::from_str::<SandboxConfig>("cpus: .nan\n").is_err());
    assert!(serde_yaml_ng::from_str::<SandboxConfig>("cpus: .inf\n").is_err());
    // So is a key from the Docker era. This test itself used to carry `image: x`,
    // ignored by serde and meaningless since the container went.
    assert!(serde_yaml_ng::from_str::<SandboxConfig>("image: x\ncpus: 2\n").is_err());
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
        Provider::api_key("${COWBOY_TEST_PROVIDER_URL}", "sk-test"),
    );
    let mut user = ModelsConfig::default();
    user.default = Some(mref("p/m"));
    user.insert(mref("p/m"), model_def("x"));

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

/// A model configured before cowboy knew its cache rate still gets the rate.
///
/// The bug this closes: `models.yaml` entries are written once and kept, so a user who
/// added a model early has `cached_input_cost_per_mtok: None` forever. Downstream that
/// falls back to the *full input* price, and for an agent — where nearly every request
/// re-sends a cached prefix — that overstates spend by roughly the cache discount. One
/// real session read $18.70 against a $2.50 bill. Shipping the rate in
/// `model_defaults.yaml` fixed nothing on its own, because resolution never consulted it.
#[test]
fn an_omitted_cached_price_is_backfilled_from_the_shipped_defaults() {
    let mut providers = ProvidersConfig::default();
    providers
        .providers
        .insert("p".into(), provider("https://api/v1"));

    // As `cowboy models add` wrote it before the rate was known: prices set, cache
    // rate absent.
    let mut def = model_def("fireworks/accounts/fireworks/models/deepseek-v4p1-flash");
    def.input_cost_per_mtok = Some(0.22);
    def.output_cost_per_mtok = Some(0.66);
    def.cached_input_cost_per_mtok = None;

    let mut user = ModelsConfig::default();
    user.default = Some(mref("p/ds"));
    user.insert(mref("p/ds"), def);

    let r = resolve_model(&providers, Some(&user), None, None).unwrap();
    let cached = r
        .cached_input_cost_per_mtok
        .expect("the shipped rate should fill the hole");
    assert!(
        cached < r.input_cost_per_mtok.unwrap(),
        "a cache read must be cheaper than fresh input, got {cached}"
    );

    // What the user *did* write is authoritative and untouched — only the hole is filled.
    assert_eq!(r.input_cost_per_mtok, Some(0.22));
    assert_eq!(r.output_cost_per_mtok, Some(0.66));
}

/// An explicit cached price always wins, including one deliberately equal to input.
#[test]
fn an_explicit_cached_price_is_never_overridden() {
    let mut providers = ProvidersConfig::default();
    providers
        .providers
        .insert("p".into(), provider("https://api/v1"));

    let mut def = model_def("fireworks/accounts/fireworks/models/deepseek-v4p1-flash");
    def.input_cost_per_mtok = Some(0.22);
    def.output_cost_per_mtok = Some(0.66);
    def.cached_input_cost_per_mtok = Some(0.123);

    let mut user = ModelsConfig::default();
    user.default = Some(mref("p/ds"));
    user.insert(mref("p/ds"), def);

    let r = resolve_model(&providers, Some(&user), None, None).unwrap();
    assert_eq!(r.cached_input_cost_per_mtok, Some(0.123));
}

#[test]
fn model_setup_pair_preserves_both_files_and_provider_permissions() {
    let tmp = tempdir();
    let providers_path = tmp.path().join(PROVIDERS_FILE);
    let models_path = tmp.path().join(MODELS_FILE);

    let state = load_model_setup_state(&providers_path, &models_path).unwrap();
    let mut providers = state.providers.clone();
    providers
        .providers
        .insert("p".into(), provider("https://api.example/v1"));
    let mut models = state.models.clone();
    models.default = Some(mref("p/m"));
    models.insert(mref("p/m"), model_def("model/id"));
    commit_model_setup_pair(
        &providers_path,
        &models_path,
        &state.generation,
        &providers,
        &models,
    )
    .unwrap();

    assert_eq!(ProvidersConfig::load(&providers_path).unwrap(), providers);
    assert_eq!(ModelsConfig::load(&models_path).unwrap(), models);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&providers_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn model_setup_generation_refuses_a_concurrent_edit() {
    let tmp = tempdir();
    let providers_path = tmp.path().join(PROVIDERS_FILE);
    let models_path = tmp.path().join(MODELS_FILE);
    let state = load_model_setup_state(&providers_path, &models_path).unwrap();

    std::fs::write(&models_path, "version: 1\nmodels: {}\n# hand edit\n").unwrap();
    let error = commit_model_setup_pair(
        &providers_path,
        &models_path,
        &state.generation,
        &state.providers,
        &state.models,
    )
    .unwrap_err();
    assert!(error.to_string().contains("changed while setup was open"));
    assert!(std::fs::read_to_string(&models_path)
        .unwrap()
        .contains("# hand edit"));
}

#[test]
fn model_setup_strictly_rejects_a_malformed_existing_file() {
    let tmp = tempdir();
    let providers_path = write(tmp.path(), PROVIDERS_FILE, "providers: [not-a-map]\n");
    let models_path = tmp.path().join(MODELS_FILE);
    assert!(matches!(
        load_model_setup_state(&providers_path, &models_path),
        Err(Error::ConfigParse { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(providers_path).unwrap(),
        "providers: [not-a-map]\n"
    );
}

#[test]
fn model_setup_recovers_an_interrupted_prepared_pair() {
    let tmp = tempdir();
    let providers_path = tmp.path().join(PROVIDERS_FILE);
    let models_path = tmp.path().join(MODELS_FILE);
    let mut old_providers = ProvidersConfig::default();
    old_providers
        .providers
        .insert("old".into(), provider("https://old.example/v1"));
    old_providers.save(&providers_path).unwrap();
    let mut old_models = ModelsConfig::default();
    old_models.default = Some(mref("old/old"));
    old_models.insert(mref("old/old"), model_def("old/model"));
    old_models.save(&models_path).unwrap();

    std::fs::copy(
        &providers_path,
        tmp.path().join(".model-setup.providers.backup"),
    )
    .unwrap();
    std::fs::copy(&models_path, tmp.path().join(".model-setup.models.backup")).unwrap();
    std::fs::write(&providers_path, "version: 1\nproviders: {}\n").unwrap();
    std::fs::write(&models_path, "version: 1\nmodels: {}\n").unwrap();
    std::fs::write(
        tmp.path().join(".model-setup.transaction"),
        "phase: prepared\nproviders_existed: true\nmodels_existed: true\n",
    )
    .unwrap();

    let recovered = load_model_setup_state(&providers_path, &models_path).unwrap();
    assert_eq!(recovered.providers, old_providers);
    assert_eq!(recovered.models, old_models);
    assert!(!tmp.path().join(".model-setup.transaction").exists());
}

// --- provider `api` / `auth` ---

#[test]
fn a_legacy_providers_file_parses_and_writes_back_unchanged() {
    let tmp = tempdir();
    let yaml = "version: 1\nproviders:\n  p:\n    base_url: https://x/v1\n    api_key: k\n";
    let p = write(tmp.path(), PROVIDERS_FILE, yaml);
    let cfg = ProvidersConfig::load(&p).unwrap();
    let prov = &cfg.providers["p"];
    assert_eq!(prov.api, WireApi::ChatCompletions);
    assert_eq!(prov.auth, ProviderAuth::ApiKey);
    // Defaults are not written, so an older cowboy can still read the file.
    let out = serde_yaml_ng::to_string(&cfg).unwrap();
    assert!(!out.contains("api:") && !out.contains("auth:"), "{out}");
}

#[test]
fn provider_api_and_auth_parse_and_resolve() {
    let tmp = tempdir();
    let p = write(
        tmp.path(),
        PROVIDERS_FILE,
        "providers:\n  bifrost:\n    base_url: http://localhost:8080/v1\n    api: responses\n  \
         chatgpt:\n    base_url: https://api.openai.com/v1\n    api: responses\n    auth: chatgpt\n",
    );
    let providers = ProvidersConfig::load(&p).unwrap();
    assert_eq!(providers.providers["bifrost"].api, WireApi::Responses);
    assert_eq!(
        providers.providers["bifrost"].api_key, "",
        "a keyless gateway is allowed"
    );
    assert_eq!(providers.providers["chatgpt"].auth, ProviderAuth::Chatgpt);

    let mut user = ModelsConfig::default();
    user.default = Some(mref("chatgpt/g"));
    user.insert(mref("chatgpt/g"), model_def("gpt-x"));
    let r = resolve_model(&providers, Some(&user), None, None).unwrap();
    assert_eq!(r.provider_name, "chatgpt");
    assert_eq!(r.api, WireApi::Responses);
    assert_eq!(r.auth, ResolvedAuth::Chatgpt);
}

#[test]
fn impossible_provider_combinations_are_refused_at_load() {
    let tmp = tempdir();
    let chat = write(
        tmp.path(),
        PROVIDERS_FILE,
        "providers:\n  c:\n    base_url: https://api.openai.com/v1\n    auth: chatgpt\n",
    );
    let err = ProvidersConfig::load(&chat).unwrap_err().to_string();
    assert!(err.contains("requires `api: responses`"), "{err}");

    let keyed = write(
        tmp.path(),
        PROVIDERS_FILE,
        "providers:\n  c:\n    base_url: https://api.openai.com/v1\n    api: responses\n    \
         auth: chatgpt\n    api_key: sk-x\n",
    );
    let err = ProvidersConfig::load(&keyed).unwrap_err().to_string();
    assert!(err.contains("takes no `api_key`"), "{err}");

    let slash = write(
        tmp.path(),
        PROVIDERS_FILE,
        "providers:\n  a/b:\n    base_url: https://x/v1\n",
    );
    assert!(ProvidersConfig::load(&slash).is_err());

    let typo = write(
        tmp.path(),
        PROVIDERS_FILE,
        "providers:\n  c:\n    base_url: https://x/v1\n    api: grpc\n",
    );
    assert!(ProvidersConfig::load(&typo).is_err());
}

#[test]
fn a_resolved_key_never_appears_in_debug_output() {
    let dbg = format!("{:?}", ResolvedAuth::ApiKey("sk-very-secret".into()));
    assert!(!dbg.contains("sk-very-secret"), "{dbg}");
}

// --- `provider/name` model references ---

#[test]
fn model_refs_are_provider_slash_name() {
    let r = ModelRef::parse("bifrost/glm-4.7").unwrap();
    assert_eq!(
        (r.provider.as_str(), r.name.as_str()),
        ("bifrost", "glm-4.7")
    );
    assert_eq!(r.to_string(), "bifrost/glm-4.7");
    // Interior spaces have always been legal in display-style names.
    assert_eq!(
        ModelRef::parse("p/Cerebras: GLM 4.7").unwrap().name,
        "Cerebras: GLM 4.7"
    );
    // Bare names point at the migration.
    let err = ModelRef::parse("glm").unwrap_err().to_string();
    assert!(err.contains("cowboy models migrate"), "{err}");
    // Empty parts, a second `/`, surrounding space inside a part.
    for bad in ["/glm", "bifrost/", "a/b/c", "a /c", "/"] {
        assert!(ModelRef::parse(bad).is_err(), "{bad:?} should be refused");
    }
}

#[test]
fn the_grouped_layout_parses_and_round_trips() {
    let tmp = tempdir();
    let p = write(
        tmp.path(),
        MODELS_FILE,
        "default: chatgpt/gpt\nsummarizer: bifrost/glm\nmodels:\n  bifrost:\n    glm:\n      \
         model: zai/glm-4.7\n  chatgpt:\n    gpt:\n      model: gpt-6.1-sol\n",
    );
    let cfg = ModelsConfig::load(&p).unwrap();
    assert_eq!(cfg.default, Some(mref("chatgpt/gpt")));
    assert_eq!(cfg.get(&mref("bifrost/glm")).unwrap().model, "zai/glm-4.7");
    let names: Vec<String> = cfg.iter().map(|(r, _)| r.to_string()).collect();
    assert_eq!(names, ["bifrost/glm", "chatgpt/gpt"]);
    let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
    assert_eq!(serde_yaml_ng::from_str::<ModelsConfig>(&yaml).unwrap(), cfg);
}

#[test]
fn the_old_flat_layout_is_refused_with_the_migrate_hint() {
    let tmp = tempdir();
    for old in [
        "default: m\nmodels:\n  m:\n    provider: p\n    model: x\n",
        "models:\n  m:\n    provider: p\n    model: x\n",
        "default: m\nmodels: {}\n",
    ] {
        let p = write(tmp.path(), MODELS_FILE, old);
        let err = ModelsConfig::load(&p).unwrap_err().to_string();
        assert!(err.contains("cowboy models migrate"), "{old:?}: {err}");
    }
}

#[test]
fn project_models_override_user_models_by_qualified_name() {
    let mut providers = ProvidersConfig::default();
    providers
        .providers
        .insert("a".into(), provider("https://a/v1"));
    providers
        .providers
        .insert("b".into(), provider("https://b/v1"));

    let mut user = ModelsConfig::default();
    user.default = Some(mref("a/m"));
    user.insert(mref("a/m"), model_def("user-a"));
    user.insert(mref("b/m"), model_def("user-b"));
    let mut project = ModelsConfig::default();
    project.insert(mref("b/m"), model_def("project-b"));

    // Same name on two providers stays two models; the project overrides only `b/m`.
    let r = resolve_model(&providers, Some(&user), Some(&project), None).unwrap();
    assert_eq!(
        (r.model.as_str(), r.base_url.as_str()),
        ("user-a", "https://a/v1")
    );
    let r = resolve_model(&providers, Some(&user), Some(&project), Some(&mref("b/m"))).unwrap();
    assert_eq!(
        (r.model.as_str(), r.provider_name.as_str()),
        ("project-b", "b")
    );

    // A model under a provider the home file does not define is refused, naming both.
    project.insert(mref("rogue/m"), model_def("x"));
    let err = resolve_model(
        &providers,
        Some(&user),
        Some(&project),
        Some(&mref("rogue/m")),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("rogue") && err.contains("a, b"), "{err}");
}

#[test]
fn a_model_spec_carries_an_optional_level() {
    use cowboy_core::config::{ModelSpec, ReasoningEffort as R};
    let s = ModelSpec::parse("chatgpt/gpt-6.1-sol@xhigh").unwrap();
    assert_eq!(s.model.to_string(), "chatgpt/gpt-6.1-sol");
    assert_eq!(s.reasoning, Some(R::Xhigh));
    assert_eq!(s.to_string(), "chatgpt/gpt-6.1-sol@xhigh");
    // `ultra` is read as `max`.
    assert_eq!(
        ModelSpec::parse("a/b@ultra").unwrap().reasoning,
        Some(R::Max)
    );
    let s = ModelSpec::parse("default/glm").unwrap();
    assert_eq!(
        (s.model.to_string(), s.reasoning),
        ("default/glm".into(), None)
    );
    // A suffix that is not a level is part of the name.
    assert_eq!(ModelSpec::split("a/b@v2"), ("a/b@v2", None));
    assert!(
        ModelSpec::parse("bare@high").is_err(),
        "the model part needs a provider"
    );
    let y: ModelSpec = serde_yaml_ng::from_str("a/b@medium").unwrap();
    assert_eq!(serde_yaml_ng::to_string(&y).unwrap().trim(), "a/b@medium");
}

#[test]
fn a_level_is_stepped_to_one_the_model_accepts() {
    use cowboy_core::config::{clamp_reasoning, lowest_reasoning, ReasoningEffort as R};
    let accepted = [R::Low, R::Medium, R::Xhigh];
    assert_eq!(clamp_reasoning(R::High, &accepted), R::Medium, "down first");
    assert_eq!(clamp_reasoning(R::Max, &accepted), R::Xhigh);
    assert_eq!(
        clamp_reasoning(R::Minimal, &accepted),
        R::Low,
        "up when below all"
    );
    assert_eq!(clamp_reasoning(R::Xhigh, &accepted), R::Xhigh);
    assert_eq!(
        clamp_reasoning(R::Max, &[]),
        R::Max,
        "unknown: sent as asked"
    );
    assert_eq!(lowest_reasoning(&accepted), R::Low);
    assert_eq!(lowest_reasoning(&[]), R::Minimal);
}
