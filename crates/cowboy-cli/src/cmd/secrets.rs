//! `cowboy secrets` — grant host credentials into the container.
//!
//! Grants come from three host-owned sources, merged at session start:
//! the repo's `.cowboy/security.yaml` (committed, opinionated), and your personal
//! overlay under `~/.config/cowboy/secrets/` — `global.yaml` (all projects) and
//! `projects/<key>.yaml` (this repository — keyed by the main repo, so it applies
//! to every worktree). The agent can write none of these. `add` writes your
//! personal overlay by default (so personal paths don't end up in the repo);
//! `--repo` instead prints a snippet to paste.

use anyhow::{Context, Result};
use cowboy_core::config::{expand_path, ConfigPaths, SecretEnv, SecretMount, SecurityConfig};
use cowboy_core::presets::{preset, NAMES as PRESETS};
use cowboy_core::usersecrets;

use crate::cli::{SecretsAddArgs, SecretsCommand};
use crate::project::repo_key;
use crate::style;

pub fn run(command: SecretsCommand) -> Result<()> {
    match command {
        SecretsCommand::List => list(),
        SecretsCommand::Add(args) => add(args),
    }
}

/// The merge key for the current repository (matches the agent's). Keyed by the
/// repo (not the worktree), so a grant applies to every worktree of the repo.
fn project_key() -> Result<String> {
    let root = crate::cmd::project_root()?;
    let canon = std::fs::canonicalize(&root).unwrap_or(root);
    Ok(repo_key(&canon))
}

/// Grants gathered from a preset and/or explicit flags.
#[derive(Default)]
struct Collected {
    env: Vec<(String, String)>,     // (name, source_env)
    env_cmd: Vec<(String, String)>, // (name, source_command)
    files: Vec<(String, String)>,   // (source, target)
    /// Host sources whose existing file grants this add supersedes (a preset's
    /// own sources and guards), so re-adding a preset upgrades an older grant.
    replaces: Vec<String>,
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
        c.env_cmd.extend(
            p.env_cmd
                .iter()
                .map(|(n, cmd)| (n.to_string(), cmd.to_string())),
        );
        c.replaces.extend(p.sources().map(str::to_string));
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
                (
                    f.clone(),
                    format!("{}/{base}", cowboy_sandbox::plan::AGENT_HOME),
                )
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
    if c.env.is_empty() && c.env_cmd.is_empty() && c.files.is_empty() {
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
    let changes = merge(&mut us, c.env, c.env_cmd, c.files, &c.replaces, c.domains);
    usersecrets::write(&path, &us)?;

    let scope = if args.global {
        "all projects"
    } else {
        "this repo (all worktrees)"
    };
    crate::ui::ok(&format!("wrote credential grant to {}", path.display()));
    println!("  applies to {scope} (merged with the repo's security.yaml at session start)");
    for line in &changes {
        println!("  {line}");
    }
    for n in &c.notes {
        println!("  {n}");
    }
    Ok(())
}

/// Fold new grants into an overlay. An env var of the same name, and a file grant
/// from one of `replaces` (or of the same source), are *replaced* rather than kept
/// beside the new one: presets change (gcloud moved from a config mount to a
/// token), and re-running `cowboy secrets add <preset>` is how a user picks that
/// up. Returns a line per replaced or removed grant, for the user.
fn merge(
    us: &mut usersecrets::UserSecrets,
    env: Vec<(String, String)>,
    env_cmd: Vec<(String, String)>,
    files: Vec<(String, String)>,
    replaces: &[String],
    domains: Vec<String>,
) -> Vec<String> {
    let mut changes = Vec::new();
    let new_env = env
        .into_iter()
        .map(|(name, source_env)| SecretEnv {
            name,
            source_env,
            source_command: None,
            required: false,
            approval: None,
        })
        .chain(env_cmd.into_iter().map(|(name, cmd)| SecretEnv {
            name,
            source_env: String::new(),
            source_command: Some(cmd),
            required: false,
            approval: None,
        }));
    for e in new_env {
        if let Some(old) = us.env.iter_mut().find(|x| x.name == e.name) {
            if old.source_env != e.source_env || old.source_command != e.source_command {
                changes.push(format!("replaced env {}", e.name));
                // The user's approval/required choices survive the upgrade.
                old.source_env = e.source_env;
                old.source_command = e.source_command;
            }
        } else {
            us.env.push(e);
        }
    }
    let new_sources: Vec<&str> = files.iter().map(|(s, _)| s.as_str()).collect();
    us.files.retain(|f| {
        let superseded = replaces.contains(&f.source) || new_sources.contains(&f.source.as_str());
        let kept = files.iter().any(|(s, t)| *s == f.source && *t == f.target);
        if superseded && !kept {
            changes.push(format!("removed file {} → {}", f.source, f.target));
        }
        !superseded || kept
    });
    for (source, target) in files {
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
    for d in domains {
        // `*.x.com` and `x.com` are the same rule; keep the one that reads right.
        if let Some(old) = us
            .allow
            .domains
            .iter_mut()
            .find(|x| x.strip_prefix("*.") == Some(d.as_str()))
        {
            *old = d;
        } else if !us.allow.domains.contains(&d) {
            us.allow.domains.push(d);
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    us.allow.domains.retain(|d| seen.insert(d.clone()));
    changes
}

/// Print a paste-ready block for the repo's committed security.yaml.
fn print_repo_snippet(c: &Collected) {
    println!("# Add the following to .cowboy/security.yaml (host-owned, never mounted).\n");
    println!("# Under `secrets:` —");
    if !c.env.is_empty() || !c.env_cmd.is_empty() {
        println!("  env:");
        for (name, src) in &c.env {
            println!("    - name: {name}");
            println!("      source_env: {src}");
        }
        for (name, cmd) in &c.env_cmd {
            println!("    - name: {name}");
            // debug-ok: `{:?}` is doing YAML quoting/escaping here, not showing a
            // Rust value — this line is part of a paste-able snippet.
            println!("      source_command: {cmd:?}");
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
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(root.clone());
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
            if let Some(cmd) = &e.source_command {
                let mark = if crate::project::source_command_ok(cmd, Some(&root)) {
                    "ok"
                } else {
                    "FAILED on host"
                };
                println!("  env   {} ← `{cmd}`  [{label}] [{mark}]", e.name);
            } else {
                let mark = if std::env::var(&e.source_env).is_ok() {
                    "set"
                } else {
                    "MISSING on host"
                };
                println!("  env   {} ← ${}  [{label}] [{mark}]", e.name, e.source_env);
            }
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
        println!(
            "{}",
            style::dim("no credential grants. Add one with `cowboy secrets add <preset>`.")
        );
        println!(
            "{}",
            style::dim(&format!("presets: {}", PRESETS.join(", ")))
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset_parts(name: &str) -> Collected {
        collect(&SecretsAddArgs {
            preset: Some(name.into()),
            ..Default::default()
        })
        .unwrap()
    }

    fn add_preset(us: &mut usersecrets::UserSecrets, name: &str) -> Vec<String> {
        let c = preset_parts(name);
        merge(us, c.env, c.env_cmd, c.files, &c.replaces, c.domains)
    }

    /// Re-adding a preset is the upgrade path: the old gcloud grant mounted the
    /// whole config dir (refresh tokens included) at the pre-move HOME, and must not
    /// survive next to the token-based one.
    #[test]
    fn re_adding_a_preset_replaces_its_older_grant() {
        let mut us = usersecrets::UserSecrets {
            files: vec![SecretMount {
                source: "~/.config/gcloud".into(),
                target: "/tmp/.config/gcloud".into(),
                read_only: true,
                required: false,
                approval: None,
            }],
            ..Default::default()
        };
        us.allow.domains = vec!["accounts.google.com".into(), "*.googleapis.com".into()];

        let changes = add_preset(&mut us, "gcloud");
        assert!(us.files.is_empty(), "{:?}", us.files);
        assert!(changes.iter().any(|c| c.contains("~/.config/gcloud")));
        let names: Vec<_> = us.env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["CLOUDSDK_AUTH_ACCESS_TOKEN", "CLOUDSDK_CORE_PROJECT"]
        );
        assert_eq!(
            us.allow.domains,
            [
                "accounts.google.com".to_string(),
                "googleapis.com".to_string()
            ]
        );

        // Idempotent: a second add changes nothing and says so.
        let before = (us.env.clone(), us.files.clone(), us.allow.domains.clone());
        assert!(add_preset(&mut us, "gcloud").is_empty());
        assert_eq!(
            before,
            (us.env.clone(), us.files.clone(), us.allow.domains.clone())
        );
    }

    #[test]
    fn a_retargeted_preset_file_is_moved_and_unrelated_grants_are_kept() {
        let mount = |source: &str, target: &str| SecretMount {
            source: source.into(),
            target: target.into(),
            read_only: true,
            required: false,
            approval: Some("required".into()),
        };
        let mut us = usersecrets::UserSecrets {
            files: vec![
                mount("~/.config/gh", "/tmp/.config/gh"),
                mount("~/.npmrc", "/home/agent/.npmrc"),
            ],
            env: vec![SecretEnv {
                name: "GH_TOKEN".into(),
                source_env: String::new(),
                source_command: Some("gh auth token".into()),
                required: true,
                approval: Some("required".into()),
            }],
            ..Default::default()
        };
        add_preset(&mut us, "gh");
        let files: Vec<_> = us
            .files
            .iter()
            .map(|f| (f.source.as_str(), f.target.as_str()))
            .collect();
        assert_eq!(
            files,
            [
                ("~/.npmrc", "/home/agent/.npmrc"),
                ("~/.config/gh", "/home/agent/.config/gh")
            ]
        );
        // Unchanged env keeps the user's own choices.
        assert!(us.env[0].required);
        assert_eq!(us.env[0].approval.as_deref(), Some("required"));
    }

    #[test]
    fn a_bare_file_defaults_into_the_agent_home() {
        let c = collect(&SecretsAddArgs {
            file: vec!["~/.npmrc".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            c.files,
            [("~/.npmrc".to_string(), "/home/agent/.npmrc".to_string())]
        );
    }
}
