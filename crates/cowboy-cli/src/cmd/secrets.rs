//! `cowboy secrets` — grant host credentials into the container.
//!
//! Grants come from three host-owned sources, merged at session start:
//! the repo's `.cowboy/security.yaml` (committed, opinionated), and your personal
//! overlay under `~/.config/cowboy/secrets/` — `global.yaml` (all projects) and
//! `projects/<key>.yaml` (this worktree, keyed like memory). The agent can write
//! none of these. `add` writes your personal overlay by default (so personal
//! paths don't end up in the repo); `--repo` instead prints a snippet to paste.

use anyhow::{Context, Result};
use cowboy_core::config::{expand_path, ConfigPaths, SecretEnv, SecretMount, SecurityConfig};
use cowboy_core::usersecrets;

use crate::cli::{SecretsAddArgs, SecretsCommand};
use crate::net::runtime::project_hash;

pub fn run(command: SecretsCommand) -> Result<()> {
    match command {
        SecretsCommand::List => list(),
        SecretsCommand::Add(args) => add(args),
    }
}

/// The merge key for the current worktree (matches the agent's).
fn project_key() -> Result<String> {
    let root = crate::cmd::project_root()?;
    let canon = std::fs::canonicalize(&root).unwrap_or(root);
    Ok(format!("{:08x}", project_hash(&canon)))
}

/// A known-tool preset: read-only file grants + the network it needs.
struct Preset {
    files: &'static [(&'static str, &'static str)],
    domains: &'static [&'static str],
    note: &'static str,
}

fn preset(name: &str) -> Option<Preset> {
    Some(match name {
        "gh" => Preset {
            files: &[("~/.config/gh", "/tmp/.config/gh")],
            domains: &["api.github.com", "github.com"],
            note: "your GitHub CLI auth (read-only).",
        },
        "gcloud" => Preset {
            files: &[("~/.config/gcloud", "/tmp/.config/gcloud")],
            domains: &[
                "accounts.google.com",
                "oauth2.googleapis.com",
                "*.googleapis.com",
            ],
            note: "your gcloud config + application-default credentials (read-only). \
                   Token refresh needs write access — set read_only: false if it fails.",
        },
        "kubectl" => Preset {
            files: &[("~/.kube", "/tmp/.kube")],
            domains: &[],
            note: "your kubeconfig (read-only). Also allow your cluster's API server host.",
        },
        "aws" => Preset {
            files: &[("~/.aws", "/tmp/.aws")],
            domains: &["*.amazonaws.com"],
            note: "your AWS credentials/config (read-only).",
        },
        "git" => Preset {
            files: &[
                ("~/.gitconfig", "/tmp/.gitconfig"),
                ("~/.git-credentials", "/tmp/.git-credentials"),
            ],
            domains: &["github.com"],
            note: "your git config + stored credentials (read-only).",
        },
        "ssh" => Preset {
            files: &[("~/.ssh", "/tmp/.ssh")],
            domains: &[],
            note: "WARNING: exposes your SSH PRIVATE KEYS to the agent (read-only).",
        },
        _ => return None,
    })
}

const PRESETS: &[&str] = &["gh", "gcloud", "kubectl", "aws", "git", "ssh"];

/// Grants gathered from a preset and/or explicit flags.
#[derive(Default)]
struct Collected {
    env: Vec<(String, String)>,   // (name, source_env)
    files: Vec<(String, String)>, // (source, target)
    domains: Vec<String>,
    notes: Vec<String>,
}

fn collect(args: &SecretsAddArgs) -> Result<Collected> {
    let mut c = Collected::default();
    if let Some(name) = &args.preset {
        let p = preset(name)
            .with_context(|| format!("unknown preset {name:?}; try: {}", PRESETS.join(", ")))?;
        c.files
            .extend(p.files.iter().map(|(s, t)| (s.to_string(), t.to_string())));
        c.domains.extend(p.domains.iter().map(|d| d.to_string()));
        c.notes.push(format!("{name}: {}", p.note));
    }
    for f in &args.file {
        let (src, target) = match f.split_once(':') {
            Some((s, t)) => (s.to_string(), t.to_string()),
            None => {
                let base = std::path::Path::new(f)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("cred");
                (f.clone(), format!("/tmp/{base}"))
            }
        };
        c.files.push((src, target));
    }
    for e in &args.env {
        let (name, src) = match e.split_once('=') {
            Some((n, s)) => (n.to_string(), s.to_string()),
            None => (e.clone(), e.clone()),
        };
        c.env.push((name, src));
    }
    if c.env.is_empty() && c.files.is_empty() {
        anyhow::bail!(
            "nothing to add; give a preset ({}) or --env/--file",
            PRESETS.join(", ")
        );
    }
    Ok(c)
}

fn add(args: SecretsAddArgs) -> Result<()> {
    let c = collect(&args)?;

    if args.repo {
        print_repo_snippet(&c);
        return Ok(());
    }

    // Write the personal overlay (per-worktree by default, or --global).
    let path = if args.global {
        usersecrets::global_file()
    } else {
        usersecrets::project_file(&project_key()?)
    }
    .context("cannot resolve home config directory")?;

    let mut us = usersecrets::read(&path);
    for (name, source_env) in c.env {
        if !us.env.iter().any(|e| e.name == name) {
            us.env.push(SecretEnv {
                name,
                source_env,
                required: false,
                approval: None,
            });
        }
    }
    for (source, target) in c.files {
        if !us
            .files
            .iter()
            .any(|f| f.source == source && f.target == target)
        {
            us.files.push(SecretMount {
                source,
                target,
                read_only: true,
                required: false,
                approval: None,
            });
        }
    }
    for d in c.domains {
        if !us.allow.domains.contains(&d) {
            us.allow.domains.push(d);
        }
    }
    usersecrets::write(&path, &us)?;

    let scope = if args.global {
        "all projects"
    } else {
        "this worktree"
    };
    println!("✓ wrote credential grant to {}", path.display());
    println!("  applies to {scope} (merged with the repo's security.yaml at session start)");
    for n in &c.notes {
        println!("  {n}");
    }
    Ok(())
}

/// Print a paste-ready block for the repo's committed security.yaml.
fn print_repo_snippet(c: &Collected) {
    println!("# Add the following to .cowboy/security.yaml (host-owned, never mounted).\n");
    println!("# Under `secrets:` —");
    if !c.env.is_empty() {
        println!("  env:");
        for (name, src) in &c.env {
            println!("    - name: {name}");
            println!("      source_env: {src}");
        }
    }
    if !c.files.is_empty() {
        println!("  files:");
        for (src, target) in &c.files {
            println!("    - source: {src}");
            println!("      target: {target}");
            println!("      read_only: true");
        }
    }
    if !c.domains.is_empty() {
        println!("\n# Under `network_policy.allow.domains:` —");
        for d in &c.domains {
            println!("    - {d}");
        }
    }
    for n in &c.notes {
        println!("\n# {n}");
    }
}

fn list() -> Result<()> {
    let key = project_key()?;
    let paths = ConfigPaths::for_root(crate::cmd::project_root()?);
    let repo = SecurityConfig::load(&paths.security).ok();
    let global = usersecrets::global_file()
        .map(|p| usersecrets::read(&p))
        .unwrap_or_default();
    let proj = usersecrets::project_file(&key)
        .map(|p| usersecrets::read(&p))
        .unwrap_or_default();

    let repo_env = repo
        .as_ref()
        .map(|s| s.secrets.env.clone())
        .unwrap_or_default();
    let repo_files = repo
        .as_ref()
        .map(|s| s.secrets.files.clone())
        .unwrap_or_default();

    let sources: [(&str, &[SecretEnv], &[SecretMount]); 3] = [
        ("repo", &repo_env, &repo_files),
        ("user-global", &global.env, &global.files),
        ("user-project", &proj.env, &proj.files),
    ];
    let mut any = false;
    for (label, envs, files) in sources {
        for e in envs {
            any = true;
            let mark = if std::env::var(&e.source_env).is_ok() {
                "set"
            } else {
                "MISSING on host"
            };
            println!("  env   {} ← ${}  [{label}] [{mark}]", e.name, e.source_env);
        }
        for f in files {
            any = true;
            let present = expand_path(&f.source).map(|p| p.exists()).unwrap_or(false);
            let mode = if f.read_only { "ro" } else { "rw" };
            let mark = if present {
                "present"
            } else {
                "MISSING on host"
            };
            println!(
                "  file  {} → {}  [{label}] [{mode}] [{mark}]",
                f.source, f.target
            );
        }
    }
    if !any {
        println!("no credential grants. Add one with `cowboy secrets add <preset>`.");
        println!("presets: {}", PRESETS.join(", "));
    }
    Ok(())
}
