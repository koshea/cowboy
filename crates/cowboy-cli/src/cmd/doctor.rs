//! `cowboy doctor` — environment and configuration checks.

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use cowboy_core::config::{AgentConfig, ConfigPaths, ModelsConfig, SecurityConfig};

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

    // Network gateway prerequisites.
    r.check("nftables (nft)", check_command(&["nft", "--version"]));
    r.check("ip forwarding", check_ip_forward());

    // Config files.
    r.check("security.yaml", check_security(&paths.security));
    r.check("agent.yaml", check_agent(&paths.agent));
    r.check("models.yaml", check_models(&paths.models));

    // Compose detection.
    r.check("compose", check_compose(&root));

    println!();
    if r.failures > 0 {
        println!("{} failure(s), {} warning(s).", r.failures, r.warnings);
        anyhow::bail!("doctor found {} problem(s)", r.failures);
    }
    println!("All checks passed ({} warning(s)).", r.warnings);
    Ok(())
}

fn check_platform() -> Status {
    if cfg!(target_os = "linux") {
        Status::Ok(std::env::consts::OS.to_string())
    } else {
        Status::Fail(format!(
            "{} is not supported; the MVP is Linux-only",
            std::env::consts::OS
        ))
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

fn check_ip_forward() -> Status {
    match std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward") {
        Ok(v) if v.trim() == "1" => Status::Ok("enabled".to_string()),
        Ok(_) => Status::Warn("disabled; the gateway will enable it for its container".to_string()),
        Err(_) => Status::Warn("could not read /proc/sys/net/ipv4/ip_forward".to_string()),
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
            Status::Fail("missing; run `cowboy init`".to_string())
        }
        Err(e) => Status::Fail(e.to_string()),
    }
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

fn check_models(path: &Path) -> Status {
    match ModelsConfig::load(path) {
        Ok(cfg) => match cfg.resolve(None) {
            Ok(profile) => {
                if std::env::var(&profile.api_key_env).is_ok() {
                    Status::Ok(format!(
                        "default={}, model={}",
                        cfg.models.default, profile.model
                    ))
                } else {
                    Status::Warn(format!(
                        "default={}, but ${} is not set",
                        cfg.models.default, profile.api_key_env
                    ))
                }
            }
            Err(e) => Status::Fail(e.to_string()),
        },
        Err(cowboy_core::Error::ConfigNotFound(_)) => {
            Status::Fail("missing; run `cowboy init`".to_string())
        }
        Err(e) => Status::Fail(e.to_string()),
    }
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
