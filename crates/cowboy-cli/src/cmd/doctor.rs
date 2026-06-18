//! `cowboy doctor` — environment and configuration checks.

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use cowboy_core::config::{
    resolve_model, AgentConfig, ConfigPaths, ModelsConfig, ProvidersConfig, SecurityConfig,
};

use crate::net::compose;

/// Outcome of a single check.
enum Status {
    Ok(String),
    Warn(String),
    Fail(String),
}

struct Report {
    failures: usize,
    warnings: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            failures: 0,
            warnings: 0,
        }
    }

    fn check(&mut self, label: &str, status: Status) {
        let (sym, msg) = match status {
            Status::Ok(m) => ("[ ok ]", m),
            Status::Warn(m) => {
                self.warnings += 1;
                ("[warn]", m)
            }
            Status::Fail(m) => {
                self.failures += 1;
                ("[fail]", m)
            }
        };
        println!("{sym} {label:<22} {msg}");
    }
}

pub async fn run() -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);
    let mut r = Report::new();

    println!("cowboy doctor — {}\n", root.display());

    // Platform.
    r.check("platform", check_platform());

    // Docker.
    r.check("docker", check_command(&["docker", "--version"]));
    r.check(
        "docker compose",
        check_command(&["docker", "compose", "version"]),
    );

    // Network gateway.
    r.check("network enforcement", check_enforcement());

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

    // Container images.
    r.check("agent image", check_image(&paths.security));
    r.check(
        "gateway image",
        image_present(
            crate::net::gateway::GATEWAY_IMAGE,
            "missing; pulled from GHCR on first run (or `docker/build.sh gateway`)",
        ),
    );

    // Compose detection.
    r.check("compose", check_compose(&root));

    // Coordination daemon.
    r.check("cowboyd", check_daemon().await);

    println!();
    if r.failures > 0 {
        println!("{} failure(s), {} warning(s).", r.failures, r.warnings);
        anyhow::bail!("doctor found {} problem(s)", r.failures);
    }
    println!("All checks passed ({} warning(s)).", r.warnings);

    // Offer Compose network approval (interactive only; no-op otherwise).
    compose::prompt_and_persist(&root)?;
    Ok(())
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
        // The gateway runs as a sidecar in the agent's netns inside Docker
        // Desktop's Linux VM, so macOS is fully supported (no host nft needed).
        "macos" => {
            Status::Ok("macos (via Docker Desktop; gateway runs as an in-VM sidecar)".to_string())
        }
        other => Status::Warn(format!(
            "{other} is untested; supported hosts are Linux and macOS (Docker Desktop)"
        )),
    }
}

fn check_command(argv: &[&str]) -> Status {
    match Command::new(argv[0]).args(&argv[1..]).output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout);
            Status::Ok(v.lines().next().unwrap_or("").trim().to_string())
        }
        Ok(out) => Status::Fail(format!("`{}` exited with {}", argv.join(" "), out.status)),
        Err(_) => Status::Fail(format!("`{}` not found", argv[0])),
    }
}

/// The nft REDIRECT that forces agent egress through the proxy runs *inside* the
/// gateway sidecar (sharing the agent's netns), using the Docker VM kernel's
/// netfilter — not the host. So there's no host `nft`/`ip_forward` requirement on
/// either platform; the gateway image (checked separately) carries the tooling.
fn check_enforcement() -> Status {
    Status::Ok("in-netns gateway sidecar (no host nft/ip-forwarding needed)".to_string())
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
            Status::Fail("missing; run `cowboy init`".to_string())
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
        Err(_) => return Status::Ok("none".into()),
    };
    // Include the user's personal overlay (global + per-repo, all worktrees).
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    cowboy_core::usersecrets::merge_into(&mut cfg, &crate::net::runtime::repo_key(&canon));
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
            Status::Fail("missing; run `cowboy init`".to_string())
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
        Ok(cfg) if cfg.providers.is_empty() => {
            Status::Warn("none configured; run `cowboy models setup`".to_string())
        }
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
        return Status::Warn("no provider; run `cowboy models setup`".to_string());
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
        Err(e) => Status::Warn(e.to_string()),
    }
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

/// Report whether a docker image exists locally (clean message, no JSON).
fn image_present(image: &str, warn: &str) -> Status {
    match std::process::Command::new("docker")
        .args(["image", "inspect", image])
        .output()
    {
        Ok(out) if out.status.success() => Status::Ok(format!("{image} present")),
        Ok(_) => Status::Warn(warn.to_string()),
        Err(_) => Status::Fail("`docker` not found".to_string()),
    }
}

fn check_image(security: &Path) -> Status {
    let image = SecurityConfig::load(security)
        .map(|c| c.container.image)
        .unwrap_or_else(|_| cowboy_core::config::ContainerConfig::default().image);
    image_present(
        &image,
        &format!("{image} missing; pulled from GHCR on first run (or `docker/build.sh agent`)"),
    )
}

fn check_compose(root: &Path) -> Status {
    match compose::detect(root) {
        Ok(Some(p)) => Status::Ok(format!(
            "{} ({} service(s); default net `{}`)",
            p.path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            p.services.len(),
            p.default_network
        )),
        Ok(None) => Status::Ok("no compose file detected".to_string()),
        Err(e) => Status::Warn(format!("found but unparsable: {e}")),
    }
}
