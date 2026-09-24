//! `cowboy doctor` — environment and configuration checks.

use std::path::Path;

use anyhow::Result;
use cowboy_core::config::{
    resolve_model, AgentConfig, ConfigPaths, ModelsConfig, ProvidersConfig, SecurityConfig,
};

use crate::style;

/// Outcome of a single check.
enum Status {
    Ok(String),
    Warn(String),
    Fail(String),
    Missing(String),
    Skipped(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    Host,
    Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Remedy {
    Host,
    Init,
    ModelsSetup,
    Fix,
}

#[derive(Debug)]
struct Failure {
    label: String,
    category: Category,
    remedy: Remedy,
}

struct Report {
    failures: Vec<Failure>,
    warnings: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            failures: Vec::new(),
            warnings: 0,
        }
    }

    fn check(&mut self, label: &str, category: Category, remedy: Remedy, status: Status) {
        let (tag, msg) = match status {
            Status::Ok(message) => (style::success("[ ok ]"), message),
            Status::Warn(message) => {
                self.warnings += 1;
                (style::warning("[warn]"), message)
            }
            Status::Fail(message) | Status::Missing(message) => {
                self.failures.push(Failure {
                    label: label.to_string(),
                    category,
                    remedy,
                });
                (style::error("[fail]"), message)
            }
            Status::Skipped(message) => (style::dim("[skip]"), message),
        };
        println!("{tag} {label:<22} {}", style::dim(&msg));
    }
}

pub async fn run() -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);
    let mut report = Report::new();

    println!(
        "{} {}\n",
        style::bold("cowboy doctor"),
        style::dim(&format!("— {}", root.display()))
    );

    report.check("platform", Category::Host, Remedy::Host, check_platform());

    println!("\n{}", style::bold("sandbox"));
    for requirement in crate::sandbox::preflight::check_all() {
        report.check(
            requirement.name,
            Category::Host,
            Remedy::Host,
            sandbox_status(requirement),
        );
    }

    // Load each independent file exactly once. Dependent checks consume these results
    // rather than trying the same failed parse again and misreporting its consequences.
    let security = SecurityConfig::load(&paths.security);
    let agent = AgentConfig::load_opt(&paths.agent);
    let providers = ProvidersConfig::load_global();
    let user_models = match ModelsConfig::user_path() {
        Some(path) => ModelsConfig::load_opt(&path),
        None => Err(cowboy_core::Error::Invalid(
            "cannot resolve home config directory".into(),
        )),
    };
    let project_models = ModelsConfig::load_opt(&paths.models);

    println!("\n{}", style::bold("configuration"));
    report.check(
        "security.yaml",
        Category::Config,
        security_remedy(&security),
        check_security(&security),
    );
    report.check(
        "agent.yaml",
        Category::Config,
        Remedy::Fix,
        check_agent(&agent),
    );
    report.check(
        "providers",
        Category::Config,
        providers_remedy(&providers),
        check_providers(&providers),
    );
    report.check(
        "models",
        Category::Config,
        models_remedy(&user_models, &project_models),
        check_models(&providers, &user_models, &project_models),
    );
    report.check(
        "config separation",
        Category::Config,
        Remedy::Fix,
        check_config_separation(&security),
    );
    report.check(
        "credential grants",
        Category::Config,
        Remedy::Fix,
        check_credentials(&security, &root),
    );

    println!("\n{}", style::bold("daemon"));
    report.check(
        "cowboyd",
        Category::Host,
        Remedy::Host,
        check_daemon().await,
    );

    println!();
    if !report.failures.is_empty() {
        println!(
            "{}",
            style::error(&format!(
                "{} failure(s), {} warning(s).",
                report.failures.len(),
                report.warnings
            ))
        );
        for line in verdict(&report) {
            println!("  {line}");
        }
        return Err(crate::AlreadyReported.into());
    }

    let summary = format!("All checks passed ({} warning(s)).", report.warnings);
    println!(
        "{}",
        if report.warnings > 0 {
            style::warning(&summary)
        } else {
            style::success(&summary)
        }
    );
    Ok(())
}

fn security_remedy(result: &cowboy_core::Result<SecurityConfig>) -> Remedy {
    match result {
        Err(cowboy_core::Error::ConfigNotFound(_)) => Remedy::Init,
        _ => Remedy::Fix,
    }
}

fn providers_remedy(result: &cowboy_core::Result<ProvidersConfig>) -> Remedy {
    match result {
        Ok(config) if config.providers.is_empty() => Remedy::ModelsSetup,
        _ => Remedy::Fix,
    }
}

fn models_remedy(
    user: &cowboy_core::Result<Option<ModelsConfig>>,
    project: &cowboy_core::Result<Option<ModelsConfig>>,
) -> Remedy {
    if user.is_err() || project.is_err() {
        Remedy::Fix
    } else {
        Remedy::ModelsSetup
    }
}

fn sandbox_status(requirement: crate::sandbox::preflight::Requirement) -> Status {
    use crate::sandbox::preflight::State;
    let message = match &requirement.remedy {
        Some(remedy) => format!("{} → {remedy}", requirement.detail),
        None => requirement.detail.clone(),
    };
    match requirement.state {
        State::Ok => Status::Ok(message),
        State::Warn => Status::Warn(message),
        State::Missing => Status::Fail(message),
    }
}

async fn check_daemon() -> Status {
    use cowboy_core::daemonproto::{DaemonReq, DaemonResp};
    match crate::cmd::daemon::request(DaemonReq::Ping).await {
        Ok(DaemonResp::Pong {
            version, sessions, ..
        }) => Status::Ok(format!("running (v{version}, {sessions} session(s))")),
        _ => Status::Warn("not running (auto-starts on the next `cowboy` session)".into()),
    }
}

fn check_platform() -> Status {
    match std::env::consts::OS {
        "linux" => Status::Ok("linux".into()),
        // The release and architecture are the sandbox checks' to judge.
        "macos" => Status::Ok("macos".into()),
        other => Status::Fail(format!(
            "{other} is not supported: the sandbox is built on Linux namespaces or macOS \
             Seatbelt"
        )),
    }
}

fn check_security(result: &cowboy_core::Result<SecurityConfig>) -> Status {
    match result {
        Ok(config) => {
            let warnings = config.warnings();
            if warnings.is_empty() {
                Status::Ok(format!(
                    "v{}, policy={:?}",
                    config.version, config.network_policy.default_external
                ))
            } else {
                Status::Warn(warnings.join("; "))
            }
        }
        Err(cowboy_core::Error::ConfigNotFound(_)) => {
            Status::Missing("missing; run `cowboy init`".into())
        }
        Err(error) => Status::Fail(error.to_string()),
    }
}

fn check_agent(result: &cowboy_core::Result<Option<AgentConfig>>) -> Status {
    match result {
        Ok(Some(config)) => Status::Ok(format!(
            "timeout={}s, max_iter={}",
            config.agent.command_timeout_seconds, config.agent.max_iterations
        )),
        Ok(None) => Status::Ok("absent; using built-in defaults".into()),
        Err(error) => Status::Fail(error.to_string()),
    }
}

fn check_providers(result: &cowboy_core::Result<ProvidersConfig>) -> Status {
    let path = ProvidersConfig::global_path();
    match result {
        Ok(config) if config.providers.is_empty() => {
            Status::Fail("none configured; run `cowboy models setup`".into())
        }
        Ok(_)
            if path
                .as_deref()
                .is_some_and(ProvidersConfig::perms_are_loose) =>
        {
            let path = path.expect("checked above");
            Status::Warn(format!(
                "{} is readable by group/other; run `chmod 600 {}`",
                path.display(),
                path.display()
            ))
        }
        Ok(config) => Status::Ok(match path {
            Some(path) => format!("{} configured ({})", config.providers.len(), path.display()),
            None => format!("{} configured", config.providers.len()),
        }),
        Err(error) => Status::Fail(error.to_string()),
    }
}

fn check_models(
    providers: &cowboy_core::Result<ProvidersConfig>,
    user: &cowboy_core::Result<Option<ModelsConfig>>,
    project: &cowboy_core::Result<Option<ModelsConfig>>,
) -> Status {
    let user = match user {
        Ok(config) => config.as_ref(),
        Err(error) => return Status::Fail(error.to_string()),
    };
    let project = match project {
        Ok(config) => config.as_ref(),
        Err(error) => return Status::Fail(error.to_string()),
    };
    let providers = match providers {
        Ok(config) if !config.providers.is_empty() => config,
        Ok(_) => return Status::Skipped("provider prerequisite failed (see above)".into()),
        Err(_) => return Status::Skipped("providers.yaml did not load (see above)".into()),
    };
    match resolve_model(providers, user, project, None) {
        Ok(model) => Status::Ok(format!(
            "default resolves to {} @ {}",
            model.model, model.base_url
        )),
        Err(error) => Status::Fail(format!(
            "{error}; add one with `cowboy models add <model-id>`"
        )),
    }
}

fn check_config_separation(result: &cowboy_core::Result<SecurityConfig>) -> Status {
    match result {
        Ok(_) => Status::Ok("security.yaml is host-only (masked, never mounted)".into()),
        Err(_) => Status::Skipped("security.yaml did not load (see above)".into()),
    }
}

/// Verify configured credential grants resolve on the host. A command-backed secret
/// is checked when the session starts; doctor must not mistake its intentionally empty
/// `source_env` for a missing environment variable.
fn check_credentials(result: &cowboy_core::Result<SecurityConfig>, root: &Path) -> Status {
    use cowboy_core::config::expand_path;

    let mut config = match result {
        Ok(config) => config.clone(),
        Err(_) => return Status::Skipped("security.yaml did not load (see above)".into()),
    };
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    cowboy_core::usersecrets::merge_into(&mut config, &crate::project::repo_key(&canonical));

    let (mut count, mut warnings, mut failures) = (0usize, Vec::new(), Vec::new());
    for env in &config.secrets.env {
        count += 1;
        if env.source_command.is_some() {
            continue;
        }
        if std::env::var(&env.source_env).is_err() {
            let message = format!("env {} missing (set ${})", env.name, env.source_env);
            if env.required {
                failures.push(message);
            } else {
                warnings.push(message);
            }
        }
    }
    for file in &config.secrets.files {
        count += 1;
        match expand_path(&file.source) {
            Ok(path) if path.exists() => {
                if world_readable(&path) {
                    warnings.push(format!("{} is world-readable", file.source));
                }
            }
            _ => {
                let message = format!("{} missing on host", file.source);
                if file.required {
                    failures.push(message);
                } else {
                    warnings.push(message);
                }
            }
        }
    }
    if !failures.is_empty() {
        Status::Fail(failures.join("; "))
    } else if !warnings.is_empty() {
        Status::Warn(warnings.join("; "))
    } else if count == 0 {
        Status::Ok("none".into())
    } else {
        Status::Ok(format!("{count} grant(s), all present"))
    }
}

#[cfg(unix)]
fn world_readable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o004 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn world_readable(_path: &Path) -> bool {
    false
}

/// Explain configuration and host failures separately using explicit categories.
fn verdict(report: &Report) -> Vec<String> {
    let mut output = Vec::new();
    let config: Vec<&Failure> = report
        .failures
        .iter()
        .filter(|failure| failure.category == Category::Config)
        .collect();
    let host: Vec<&Failure> = report
        .failures
        .iter()
        .filter(|failure| failure.category == Category::Host)
        .collect();

    let fixes: Vec<&str> = config
        .iter()
        .filter(|failure| failure.remedy == Remedy::Fix)
        .map(|failure| failure.label.as_str())
        .collect();
    if !fixes.is_empty() {
        output.push(format!(
            "fix {} using the details above — existing config was not treated as missing",
            fixes.join(", ")
        ));
    }
    if config.iter().any(|failure| failure.remedy == Remedy::Init) {
        output.push("cowboy cannot start here yet — run `cowboy init`".into());
    }
    if config
        .iter()
        .any(|failure| failure.remedy == Remedy::ModelsSetup)
    {
        output.push("model configuration is incomplete — run `cowboy models setup`".into());
    }
    if !host.is_empty() {
        output.push(format!(
            "the sandbox cannot run on this host ({}) — see the remedies above",
            host.iter()
                .map(|failure| failure.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::preflight::{Requirement, State};

    fn req(state: State, remedy: Option<&str>) -> Requirement {
        Requirement {
            name: "landlock",
            state,
            detail: "ABI 2, but 6 is required".into(),
            remedy: remedy.map(str::to_string),
        }
    }

    fn failure(report: &mut Report, label: &str, category: Category, remedy: Remedy) {
        report.check(label, category, remedy, Status::Fail("failed".into()));
    }

    #[test]
    fn verdict_uses_explicit_categories_not_labels() {
        let mut report = Report::new();
        failure(
            &mut report,
            "credential grants",
            Category::Config,
            Remedy::Fix,
        );
        failure(&mut report, "landlock", Category::Host, Remedy::Host);
        let lines = verdict(&report);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("credential grants"), "{lines:?}");
        assert!(lines[1].contains("sandbox cannot run"), "{lines:?}");
    }

    #[test]
    fn malformed_config_is_never_answered_with_setup_or_init() {
        let mut report = Report::new();
        failure(&mut report, "providers", Category::Config, Remedy::Fix);
        let lines = verdict(&report);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("fix providers"), "{lines:?}");
        assert!(!lines[0].contains("setup"), "{lines:?}");
        assert!(!lines[0].contains("init"), "{lines:?}");
    }

    #[test]
    fn dependent_model_check_skips_a_failed_provider() {
        let providers = Err(cowboy_core::Error::Invalid("bad provider".into()));
        let models = Ok(None);
        assert!(matches!(
            check_models(&providers, &models, &models),
            Status::Skipped(_)
        ));
    }

    #[test]
    fn absent_agent_config_uses_defaults() {
        assert!(
            matches!(check_agent(&Ok(None)), Status::Ok(message) if message.contains("defaults"))
        );
    }

    #[test]
    fn missing_project_config_precedes_model_setup() {
        let mut report = Report::new();
        report.check(
            "security.yaml",
            Category::Config,
            Remedy::Init,
            Status::Missing("missing".into()),
        );
        failure(
            &mut report,
            "providers",
            Category::Config,
            Remedy::ModelsSetup,
        );
        let lines = verdict(&report);
        assert!(lines[0].contains("cowboy init"), "{lines:?}");
        assert!(lines[1].contains("models setup"), "{lines:?}");
    }

    #[test]
    fn missing_boundary_requirement_fails_and_degraded_limits_warn() {
        let mut report = Report::new();
        report.check(
            "landlock",
            Category::Host,
            Remedy::Host,
            sandbox_status(req(State::Missing, Some("newer kernel"))),
        );
        report.check(
            "resource limits",
            Category::Host,
            Remedy::Host,
            sandbox_status(req(State::Warn, Some("delegate a subtree"))),
        );
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.warnings, 1);
    }

    #[test]
    fn sandbox_remedy_is_shown_with_the_problem() {
        match sandbox_status(req(State::Missing, Some("enable CONFIG_SECURITY_LANDLOCK"))) {
            Status::Fail(message) => {
                assert!(message.contains("ABI 2"), "{message}");
                assert!(
                    message.contains("enable CONFIG_SECURITY_LANDLOCK"),
                    "{message}"
                );
            }
            _ => panic!("a missing prerequisite must fail"),
        }
    }

    #[test]
    fn this_host_reports_no_sandbox_failures() {
        let mut report = Report::new();
        for check in crate::sandbox::preflight::check_all() {
            report.check(
                check.name,
                Category::Host,
                Remedy::Host,
                sandbox_status(check),
            );
        }
        if !report.failures.is_empty() && !crate::sandbox::preflight::tests_required() {
            eprintln!(
                "skipping: the sandbox cannot run here ({} checks failed)",
                report.failures.len()
            );
            return;
        }
        assert!(report.failures.is_empty());
    }
}
