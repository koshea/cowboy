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
    /// A failure whose cause is specifically "the file is not there".
    ///
    /// Distinct from [`Status::Fail`] because it needs different advice: `cowboy init`
    /// creates a missing file, but for one that exists and is *wrong* it refuses, and
    /// `--force` would discard the user's edits. Reported identically to the reader — it
    /// is still `[fail]` — but the verdict branches on it.
    Missing(String),
}

struct Report {
    failures: usize,
    warnings: usize,
    /// Labels that failed, so the verdict can say which *kind* of problem this is.
    failed: Vec<String>,
    /// Of those, the ones that failed because the file is absent.
    absent: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Self {
            failures: 0,
            warnings: 0,
            failed: Vec::new(),
            absent: Vec::new(),
        }
    }

    fn check(&mut self, label: &str, status: Status) {
        // Colored status tag ([ ok ]/[warn]/[fail]); detail dimmed. `style` is
        // TTY-gated, so piped output stays plain.
        let (tag, msg) = match status {
            Status::Ok(m) => (style::success("[ ok ]"), m),
            Status::Warn(m) => {
                self.warnings += 1;
                (style::warning("[warn]"), m)
            }
            Status::Fail(m) => {
                self.failures += 1;
                self.failed.push(label.to_string());
                (style::error("[fail]"), m)
            }
            Status::Missing(m) => {
                self.failures += 1;
                self.failed.push(label.to_string());
                self.absent.push(label.to_string());
                (style::error("[fail]"), m)
            }
        };
        println!("{tag} {label:<22} {}", style::dim(&msg));
    }
}

pub async fn run() -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);
    let mut r = Report::new();

    println!(
        "{} {}\n",
        style::bold("cowboy doctor"),
        style::dim(&format!("— {}", root.display()))
    );

    // Platform.
    r.check("platform", check_platform());

    // The sandbox's host prerequisites. First, because everything else is
    // configuration: if these fail, nothing runs regardless of how the project is
    // set up, and a reader should see the cause before the consequences.
    println!("\n{}", style::bold("sandbox"));
    for req in crate::sandbox::preflight::check_all() {
        r.check(req.name, sandbox_status(req));
    }

    println!("\n{}", style::bold("configuration"));
    // Config files.
    r.check("security.yaml", check_security(&paths.security));
    r.check("agent.yaml", check_agent(&paths.agent));
    r.check("providers", check_providers());
    r.check("models", check_models(&paths.models));
    r.check(
        "config separation",
        check_config_separation(&paths.security),
    );
    r.check(
        "credential grants",
        check_credentials(&paths.security, &root),
    );

    println!("\n{}", style::bold("daemon"));
    r.check("cowboyd", check_daemon().await);

    println!();
    if r.failures > 0 {
        println!(
            "{}",
            style::error(&format!(
                "{} failure(s), {} warning(s).",
                r.failures, r.warnings
            ))
        );
        // Say what the failures *mean*, because the two kinds have different
        // consequences and different fixes: config gaps stop `cowboy` from starting at
        // all, kernel gaps stop the sandbox from confining anything. Reading a column
        // of `[fail]` lines and working that out is the reader's job otherwise.
        for line in verdict(&r) {
            println!("  {line}");
        }
        // Everything above *is* the report; anyhow adding "Error: doctor found N
        // problem(s)" underneath it says nothing new.
        return Err(crate::AlreadyReported.into());
    }
    let summary = format!("All checks passed ({} warning(s)).", r.warnings);
    println!(
        "{}",
        if r.warnings > 0 {
            style::warning(&summary)
        } else {
            style::success(&summary)
        }
    );

    Ok(())
}

/// Render a sandbox prerequisite as a report line, folding its remedy into the
/// message — the remedy is the useful half when something is wrong.
fn sandbox_status(req: crate::sandbox::preflight::Requirement) -> Status {
    use crate::sandbox::preflight::State;
    let msg = match &req.remedy {
        Some(r) => format!("{} → {r}", req.detail),
        None => req.detail.clone(),
    };
    match req.state {
        State::Ok => Status::Ok(msg),
        State::Warn => Status::Warn(msg),
        State::Missing => Status::Fail(msg),
    }
}

/// Ping the coordination daemon. Not running is informational (it auto-starts
/// on the next `cowboy` session), so this only ever warns.
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
        "linux" => Status::Ok("linux".to_string()),
        // The sandbox is namespaces, Landlock, seccomp and nftables — all Linux
        // kernel features with no equivalent elsewhere. macOS support went with the
        // container, deliberately; there is no VM to run this in.
        other => Status::Fail(format!(
            "{other} is not supported: the sandbox is built on Linux namespaces, \
             Landlock and nftables"
        )),
    }
}

fn check_security(path: &Path) -> Status {
    match SecurityConfig::load(path) {
        Ok(cfg) => {
            let warns = cfg.warnings();
            if warns.is_empty() {
                Status::Ok(format!(
                    "v{}, policy={:?}",
                    cfg.version, cfg.network_policy.default_external
                ))
            } else {
                Status::Warn(warns.join("; "))
            }
        }
        Err(cowboy_core::Error::ConfigNotFound(_)) => {
            Status::Missing("missing; run `cowboy init`".to_string())
        }
        Err(e) => Status::Fail(e.to_string()),
    }
}

/// Verify configured credential grants resolve on the host. Missing optional
/// grants warn; missing required ones fail; world-readable cred files warn.
fn check_credentials(path: &Path, root: &Path) -> Status {
    use cowboy_core::config::expand_path;
    let mut cfg = match SecurityConfig::load(path) {
        Ok(c) => c,
        // A missing file is a fair "nothing configured"; anything else is a config we
        // could not read, and reporting that as "none" would tell the user their
        // grants are fine when we never saw them. `check_security` names the reason.
        Err(cowboy_core::Error::ConfigNotFound(_)) => return Status::Ok("none".into()),
        Err(_) => {
            return Status::Warn("not checked: security.yaml did not parse".into());
        }
    };
    // Include the user's personal overlay (global + per-repo, all worktrees).
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    cowboy_core::usersecrets::merge_into(&mut cfg, &crate::project::repo_key(&canon));
    let (mut count, mut warns, mut fails) = (0usize, Vec::new(), Vec::new());
    for e in &cfg.secrets.env {
        count += 1;
        if std::env::var(&e.source_env).is_err() {
            let msg = format!("env {} missing (set ${})", e.name, e.source_env);
            if e.required {
                fails.push(msg);
            } else {
                warns.push(msg);
            }
        }
    }
    for f in &cfg.secrets.files {
        count += 1;
        match expand_path(&f.source) {
            Ok(p) if p.exists() => {
                if world_readable(&p) {
                    warns.push(format!("{} is world-readable", f.source));
                }
            }
            _ => {
                let msg = format!("{} missing on host", f.source);
                if f.required {
                    fails.push(msg);
                } else {
                    warns.push(msg);
                }
            }
        }
    }
    if !fails.is_empty() {
        Status::Fail(fails.join("; "))
    } else if !warns.is_empty() {
        Status::Warn(warns.join("; "))
    } else if count == 0 {
        Status::Ok("none".into())
    } else {
        Status::Ok(format!("{count} grant(s), all present"))
    }
}

#[cfg(unix)]
fn world_readable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o004 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn world_readable(_p: &Path) -> bool {
    false
}

fn check_agent(path: &Path) -> Status {
    match AgentConfig::load(path) {
        Ok(cfg) => Status::Ok(format!(
            "timeout={}s, max_iter={}",
            cfg.agent.command_timeout_seconds, cfg.agent.max_iterations
        )),
        Err(cowboy_core::Error::ConfigNotFound(_)) => {
            Status::Missing("missing; run `cowboy init`".to_string())
        }
        Err(e) => Status::Fail(e.to_string()),
    }
}

/// Providers are home-owned (`~/.config/cowboy/providers.yaml`, 0600).
fn check_providers() -> Status {
    let path = match ProvidersConfig::global_path() {
        Some(p) => p,
        None => return Status::Warn("cannot resolve home config dir".to_string()),
    };
    match ProvidersConfig::load_global() {
        // A failure, not a warning: with no provider every session ends before its
        // first model call, and `doctor` exiting 0 on such a host is the one answer
        // that is certainly wrong.
        Ok(cfg) if cfg.providers.is_empty() => {
            Status::Fail("none configured; run `cowboy models setup`".to_string())
        }
        // Loud warning if the key file lost its 0600 perms (hand-edited, restored
        // from backup, copied) — group/other can read the API keys.
        Ok(_) if ProvidersConfig::perms_are_loose(&path) => Status::Warn(format!(
            "{} is readable by group/other; run `chmod 600 {}`",
            path.display(),
            path.display()
        )),
        Ok(cfg) => Status::Ok(format!(
            "{} configured ({})",
            cfg.providers.len(),
            path.display()
        )),
        Err(e) => Status::Fail(e.to_string()),
    }
}

/// Models resolve against the home providers + user/project model lists.
fn check_models(project_path: &Path) -> Status {
    let providers = match ProvidersConfig::load_global() {
        Ok(p) => p,
        Err(e) => return Status::Fail(e.to_string()),
    };
    if providers.providers.is_empty() {
        // Already reported against `providers`; keep it quiet here rather than failing
        // twice for one cause.
        return Status::Warn("no provider (see above)".to_string());
    }
    let user = match ModelsConfig::user_path().map(|p| ModelsConfig::load_opt(&p)) {
        Some(Ok(m)) => m,
        Some(Err(e)) => return Status::Fail(e.to_string()),
        None => None,
    };
    let project = match ModelsConfig::load_opt(project_path) {
        Ok(m) => m,
        Err(e) => return Status::Fail(e.to_string()),
    };
    match resolve_model(&providers, user.as_ref(), project.as_ref(), None) {
        Ok(m) => Status::Ok(format!("default resolves to {} @ {}", m.model, m.base_url)),
        // Same reasoning as `providers`: a provider with no usable model cannot run a
        // turn, so this is a failure the exit code has to carry.
        Err(e) => Status::Fail(format!("{e}; add one with `cowboy models add <model-id>`")),
    }
}

/// One or two sentences naming what the failures prevent, and where to start.
///
/// Split by kind because the consequences differ: a config gap means `cowboy` will not
/// start, a kernel gap means the *sandbox* will not, and a user reading a column of
/// `[fail]` lines should not have to classify them to know which they have.
fn verdict(r: &Report) -> Vec<String> {
    const CONFIG: &[&str] = &["security.yaml", "agent.yaml", "providers", "models"];
    let (config, host): (Vec<&String>, Vec<&String>) =
        r.failed.iter().partition(|l| CONFIG.contains(&l.as_str()));
    let mut out = Vec::new();
    if !config.is_empty() {
        let project = |l: &str| l == "security.yaml" || l == "agent.yaml";
        let broken: Vec<&str> = config
            .iter()
            .map(|l| l.as_str())
            .filter(|l| project(l) && !r.absent.iter().any(|a| a == l))
            .collect();
        if !broken.is_empty() {
            // Present but wrong. `cowboy init` is the wrong advice here — it refuses to
            // overwrite, and `--force` would throw away whatever the user edited. The
            // detail line above already says what is wrong with it; this says where.
            out.push(format!(
                "fix {} in .cowboy/ — the problem is in the file, not that it is missing",
                broken.join(" and ")
            ));
        } else {
            // `init` first when the project itself is missing: `models setup` would
            // succeed and the next run would still fail. This is the same ordering the
            // first-run report uses, for the same reason.
            let first = if config.iter().any(|l| project(l)) {
                "cowboy init"
            } else {
                "cowboy models setup"
            };
            out.push(format!(
                "cowboy cannot start here yet — start with `{first}`"
            ));
        }
    }
    if !host.is_empty() {
        out.push(format!(
            "the sandbox cannot run on this host ({}) — see the remedies above",
            host.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

fn check_config_separation(path: &Path) -> Status {
    match SecurityConfig::load(path) {
        // load() runs validate(), which rejects mounting security.yaml/.cowboy.
        Ok(_) => Status::Ok("security.yaml is host-only (masked, never mounted)".to_string()),
        Err(cowboy_core::Error::ConfigNotFound(_)) => {
            Status::Warn("no security.yaml yet; run `cowboy init`".to_string())
        }
        Err(cowboy_core::Error::SecurityInvariant(m)) => Status::Fail(m),
        Err(e) => Status::Fail(e.to_string()),
    }
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

    #[test]
    fn the_verdict_sends_you_to_init_before_models() {
        // Both orders "work" in the sense of fixing a line, but only one order fixes the
        // session: `models setup` on an uninitialized project leaves it still unable to
        // start.
        let mut r = Report::new();
        r.check("security.yaml", Status::Missing("missing".into()));
        r.check("providers", Status::Fail("none".into()));
        let v = verdict(&r);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("cowboy init"), "{v:?}");
    }

    #[test]
    fn a_broken_config_file_is_not_answered_with_init() {
        // The misdirection this replaces: any security.yaml failure sent you to
        // `cowboy init`, which refuses to overwrite an existing file — and `--force`
        // would discard whatever you had edited. A present-but-invalid file needs fixing,
        // not scaffolding.
        let mut r = Report::new();
        r.check(
            "security.yaml",
            Status::Fail("mount source \".\" would expose host-owned secrets".into()),
        );
        let v = verdict(&r);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("fix security.yaml"), "{v:?}");
        assert!(!v[0].contains("cowboy init"), "{v:?}");
    }

    #[test]
    fn a_missing_file_and_a_broken_one_are_told_apart() {
        // Both are `[fail] security.yaml` to the reader; only the advice differs.
        let mut missing = Report::new();
        missing.check("agent.yaml", Status::Missing("missing".into()));
        assert!(verdict(&missing)[0].contains("cowboy init"));

        let mut broken = Report::new();
        broken.check("agent.yaml", Status::Fail("unknown field `timout`".into()));
        assert!(verdict(&broken)[0].contains("fix agent.yaml"));

        // A missing project file alongside a broken one: fixing the broken one is still
        // the harder half, and `init` will not touch it, so say that.
        let mut both = Report::new();
        both.check("security.yaml", Status::Fail("bad mount".into()));
        both.check("agent.yaml", Status::Missing("missing".into()));
        assert!(
            verdict(&both)[0].contains("fix security.yaml"),
            "{:?}",
            verdict(&both)
        );
    }

    #[test]
    fn config_and_host_failures_are_reported_as_different_problems() {
        // They have different consequences — one stops cowboy starting, the other stops
        // the sandbox confining — and lumping them together buries the second.
        let mut r = Report::new();
        r.check("providers", Status::Fail("none".into()));
        r.check("landlock", Status::Fail("ABI too old".into()));
        let v = verdict(&r);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].contains("cowboy models setup"), "{v:?}");
        assert!(v[1].contains("landlock"), "{v:?}");
        assert!(v[1].contains("sandbox cannot run"), "{v:?}");
    }

    #[test]
    fn a_healthy_run_has_no_verdict_to_give() {
        let mut r = Report::new();
        r.check("landlock", Status::Ok("ABI 6".into()));
        r.check("models", Status::Warn("something minor".into()));
        assert!(verdict(&r).is_empty());
    }

    /// A prerequisite the sandbox cannot run without must be a **failure**, so
    /// `doctor` exits non-zero. Downgrading it to a warning would let the command
    /// report success on a host where every session then fails to start.
    #[test]
    fn a_missing_prerequisite_fails_the_run() {
        let mut r = Report::new();
        r.check(
            "landlock",
            sandbox_status(req(State::Missing, Some("newer kernel"))),
        );
        assert_eq!(r.failures, 1);
        assert_eq!(r.warnings, 0);
    }

    /// Limits are not part of the boundary, so their absence warns and `doctor` still
    /// succeeds — the distinction between the two is the point of having both.
    #[test]
    fn a_degraded_capability_only_warns() {
        let mut r = Report::new();
        r.check(
            "resource limits",
            sandbox_status(req(State::Warn, Some("delegate a subtree"))),
        );
        assert_eq!(r.failures, 0);
        assert_eq!(r.warnings, 1);
    }

    #[test]
    fn the_remedy_is_shown_with_the_problem() {
        match sandbox_status(req(State::Missing, Some("enable CONFIG_SECURITY_LANDLOCK"))) {
            Status::Fail(msg) => {
                assert!(msg.contains("ABI 2"), "the finding: {msg}");
                assert!(
                    msg.contains("enable CONFIG_SECURITY_LANDLOCK"),
                    "and what to do about it: {msg}"
                );
            }
            _ => panic!("a missing prerequisite must fail"),
        }
    }

    /// This host runs the sandbox, so `doctor` must not report a sandbox failure on
    /// it — otherwise the command is not usable as the preflight it is meant to be.
    ///
    /// Skips where the sandbox genuinely cannot run (CI runners, macOS, a kernel
    /// without Landlock), because there the failures `doctor` reports are correct and
    /// asserting on them tests the host rather than the code.
    /// `COWBOY_SANDBOX_TESTS=required` turns the skip into a failure — the same switch
    /// the sandbox integration tests use, so one setting means "prove it" everywhere.
    #[test]
    fn this_host_reports_no_sandbox_failures() {
        let mut r = Report::new();
        for c in crate::sandbox::preflight::check_all() {
            r.check(c.name, sandbox_status(c));
        }
        if r.failures > 0 && !crate::sandbox::preflight::tests_required() {
            eprintln!(
                "skipping: the sandbox cannot run here ({} checks failed)",
                r.failures
            );
            return;
        }
        assert_eq!(
            r.failures, 0,
            "doctor reports a sandbox failure on a host that runs it"
        );
    }
}
