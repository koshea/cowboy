//! `cowboy secrets` — grant host credentials into the container.
//!
//! Grants live only in the host-owned, container-masked `.cowboy/security.yaml`
//! (the agent can't grant itself anything). `add` is non-destructive: it prints
//! a paste-ready block (the `secrets` grant plus the `network_policy.allow`
//! domains the tool needs) for the user to merge into security.yaml.

use anyhow::{Context, Result};
use cowboy_core::config::{expand_path, ConfigPaths, SecurityConfig};

use crate::cli::{SecretsAddArgs, SecretsCommand};

pub fn run(command: SecretsCommand) -> Result<()> {
    match command {
        SecretsCommand::List => list(),
        SecretsCommand::Add(args) => add(args),
    }
}

/// A known-tool preset: read-only file grants + the network it needs.
struct Preset {
    /// (host source, container target) pairs.
    files: &'static [(&'static str, &'static str)],
    /// Domains the tool talks to (suggested network_policy.allow additions).
    domains: &'static [&'static str],
    /// What the grant exposes / extra notes.
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
                   Token refresh needs write access — add read_only: false if it fails.",
        },
        "kubectl" => Preset {
            files: &[("~/.kube", "/tmp/.kube")],
            domains: &[],
            note: "your kubeconfig (read-only). Also allow your cluster's API server \
                   host:port in network_policy.allow.",
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
            note: "WARNING: this exposes your SSH PRIVATE KEYS to the agent (read-only). \
                   Only grant this if you trust the task.",
        },
        _ => return None,
    })
}

const PRESETS: &[&str] = &["gh", "gcloud", "kubectl", "aws", "git", "ssh"];

fn list() -> Result<()> {
    let paths = ConfigPaths::for_root(crate::cmd::project_root()?);
    let cfg = match SecurityConfig::load(&paths.security) {
        Ok(c) => c,
        Err(_) => {
            println!("no .cowboy/security.yaml (run `cowboy init`)");
            return Ok(());
        }
    };
    if cfg.secrets.env.is_empty() && cfg.secrets.files.is_empty() {
        println!("no credential grants configured. Add one with `cowboy secrets add <preset>`.");
        println!("presets: {}", PRESETS.join(", "));
        return Ok(());
    }
    if !cfg.secrets.env.is_empty() {
        println!("env grants:");
        for e in &cfg.secrets.env {
            let present = std::env::var(&e.source_env).is_ok();
            let mark = if present { "set" } else { "MISSING on host" };
            println!("  {} ← ${}  [{mark}]", e.name, e.source_env);
        }
    }
    if !cfg.secrets.files.is_empty() {
        println!("file grants:");
        for f in &cfg.secrets.files {
            let present = expand_path(&f.source).map(|p| p.exists()).unwrap_or(false);
            let mode = if f.read_only { "ro" } else { "rw" };
            let mark = if present {
                "present"
            } else {
                "MISSING on host"
            };
            println!("  {} → {}  [{mode}] [{mark}]", f.source, f.target);
        }
    }
    Ok(())
}

fn add(args: SecretsAddArgs) -> Result<()> {
    // Collect file + env grants and suggested domains from a preset and/or flags.
    let mut files: Vec<(String, String)> = Vec::new();
    let mut domains: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    if let Some(name) = &args.preset {
        let p = preset(name).with_context(|| {
            format!(
                "unknown preset {name:?}; try one of: {}",
                PRESETS.join(", ")
            )
        })?;
        for (s, t) in p.files {
            files.push((s.to_string(), t.to_string()));
        }
        domains.extend(p.domains.iter().map(|d| d.to_string()));
        notes.push(format!("{name}: {}", p.note));
    }
    for f in &args.file {
        // SRC or SRC:TARGET
        let (src, target) = match f.split_once(':') {
            Some((s, t)) => (s.to_string(), t.to_string()),
            None => {
                // Default target: /tmp/<basename> so it lands under HOME=/tmp.
                let base = std::path::Path::new(f)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("cred");
                (f.clone(), format!("/tmp/{base}"))
            }
        };
        files.push((src, target));
    }
    for e in &args.env {
        // NAME or NAME=HOST_ENV
        let (name, src) = match e.split_once('=') {
            Some((n, s)) => (n.to_string(), s.to_string()),
            None => (e.clone(), e.clone()),
        };
        env.push((name, src));
    }

    if files.is_empty() && env.is_empty() {
        anyhow::bail!(
            "nothing to add; give a preset ({}) or --env/--file",
            PRESETS.join(", ")
        );
    }

    println!("# Add the following to .cowboy/security.yaml (host-owned, never mounted).\n");
    println!("# Under `secrets:` —");
    if !env.is_empty() {
        println!("  env:");
        for (name, src) in &env {
            println!("    - name: {name}");
            println!("      source_env: {src}");
        }
    }
    if !files.is_empty() {
        println!("  files:");
        for (src, target) in &files {
            println!("    - source: {src}");
            println!("      target: {target}");
            println!("      read_only: true");
        }
    }
    if !domains.is_empty() {
        println!("\n# Under `network_policy.allow.domains:` —");
        for d in &domains {
            println!("    - {d}");
        }
    }
    if !notes.is_empty() {
        println!("\n# Grants:");
        for n in &notes {
            println!("#   {n}");
        }
    }
    println!(
        "\n# (env values are read from the host at runtime; file grants mount read-only.\n\
         #  The network domains are a separate, explicit decision — approve them here or\n\
         #  when the agent first connects.)"
    );
    Ok(())
}
