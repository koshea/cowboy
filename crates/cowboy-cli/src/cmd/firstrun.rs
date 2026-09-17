//! What a session needs before it can start, checked **once, in the client**.
//!
//! The old order surfaced these gaps one at a time, and in the wrong place. A fresh
//! machine got "no model provider configured; run `cowboy models setup`"; after fixing
//! that, the next run auto-started a daemon, which spawned a worker, which failed on
//! `.cowboy/security.yaml` — so the actual remedy ("run `cowboy init` first") reached
//! the user as a line buried in a *worker log tail*, one indirection away, after a
//! background process had already been started. Two dead ends, revealed in sequence,
//! the second one badly.
//!
//! So: check everything a start needs up front, and print one block naming every gap
//! with the command that fixes it.

use std::path::{Path, PathBuf};

use std::io::IsTerminal;

use anyhow::Result;
use cowboy_core::config::{resolve_model, ConfigPaths, ModelsConfig, ProvidersConfig};

use crate::style;

/// Something missing that a session cannot start without.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gap {
    /// No `.cowboy/` config for this project.
    Project { root: PathBuf },
    /// No provider in the home config (endpoint + key).
    Provider,
    /// A provider exists, but no usable model — including the "said no to the model
    /// step of `models setup`" case, which used to fail with an unactionable
    /// resolve error.
    Model { detail: String },
}

impl Gap {
    /// What is wrong, and the command that fixes it.
    fn line(&self) -> (String, String) {
        match self {
            Gap::Project { root } => (
                format!("no .cowboy/ config in {}", root.display()),
                "cowboy init".into(),
            ),
            Gap::Provider => (
                "no model provider configured".into(),
                "cowboy models setup".into(),
            ),
            Gap::Model { detail } => (detail.clone(), "cowboy models add <model-id>".into()),
        }
    }
}

/// Everything a session start needs, in the order a user would fix it.
///
/// Deliberately does not touch the daemon, the sandbox, or the network: this runs before
/// any of that exists, and its only job is to answer "can this possibly work?".
pub fn check(root: &Path) -> Vec<Gap> {
    let paths = ConfigPaths::for_root(root);
    let mut gaps = Vec::new();
    if !paths.security.is_file() {
        gaps.push(Gap::Project {
            root: root.to_path_buf(),
        });
    }
    let providers = ProvidersConfig::load_global().unwrap_or_default();
    if providers.providers.is_empty() {
        gaps.push(Gap::Provider);
        // Without a provider there is nothing to resolve a model against, so a second
        // complaint about models would just be noise.
        return gaps;
    }
    let user = ModelsConfig::user_path().and_then(|p| ModelsConfig::load_opt(&p).ok().flatten());
    let project = ModelsConfig::load_opt(&paths.models).ok().flatten();
    if let Err(e) = resolve_model(&providers, user.as_ref(), project.as_ref(), None) {
        gaps.push(Gap::Model {
            detail: format!("a provider is configured, but no usable model ({e})"),
        });
    }
    gaps
}

/// Fix what can be fixed here, or stop with the full report.
///
/// The one gap worth offering to close in place is a missing `.cowboy/`: it is by far
/// the most common first-run failure, `init` only writes two config files and some
/// gitignore lines, and the alternative is telling someone to run a command and then
/// re-run the one they just ran. The offer is deliberately narrow:
///
/// - **only when it is the sole gap** — a half-set-up machine gets the checklist, not a
///   prompt that leaves it still broken;
/// - **only in a git worktree**, so a mistyped `cd` cannot scatter `.cowboy/` into a
///   home directory or a download folder;
/// - **only with a human present** (`prompt::confirm` refuses a non-TTY), so scripts
///   keep failing loudly.
pub fn resolve_or_bail(root: &Path, gaps: &[Gap]) -> Result<()> {
    let only_project = matches!(gaps, [Gap::Project { .. }]);
    // A terminal is required explicitly rather than left to the prompt's default: this
    // branch *creates files*, and a piped run must never do that as a side effect of
    // failing.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if only_project && interactive && root.join(".git").exists() {
        println!(
            "{} {}",
            style::warning("no cowboy config in"),
            style::bold(&root.display().to_string())
        );
        if crate::prompt::confirm("Initialize it here?", true)? {
            crate::cmd::init::run(crate::cli::InitArgs {
                force: false,
                git: false,
            })?;
            return Ok(());
        }
    }
    eprint!("{}", report(root, gaps));
    // The report *is* the message; see `AlreadyReported`.
    Err(crate::AlreadyReported.into())
}

/// The block shown when a start cannot proceed: every gap, each with its remedy.
pub fn report(root: &Path, gaps: &[Gap]) -> String {
    let mut out = format!(
        "{} {}\n",
        style::error("cowboy can't start in"),
        style::bold(&root.display().to_string())
    );
    let width = gaps
        .iter()
        .map(|g| g.line().0.chars().count())
        .max()
        .unwrap_or(0);
    for gap in gaps {
        let (what, fix) = gap.line();
        out.push_str(&format!(
            "  {} {:<width$}  →  {}\n",
            style::error("✗"),
            what,
            style::bold(&fix),
            width = width
        ));
    }
    // The order above is the order to run them in, which is worth saying once rather
    // than leaving the reader to infer it from a list.
    if gaps.len() > 1 {
        out.push_str(&format!(
            "\n{}\n",
            style::dim("run them in that order, then `cowboy` again")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_project_names_the_directory_and_the_command() {
        let root = PathBuf::from("/home/dev/proj");
        let gaps = vec![Gap::Project { root: root.clone() }];
        let out = report(&root, &gaps);
        assert!(out.contains("/home/dev/proj"), "{out}");
        assert!(out.contains("cowboy init"), "{out}");
        // A single gap needs no ordering advice.
        assert!(!out.contains("in that order"), "{out}");
    }

    #[test]
    fn every_gap_is_reported_at_once_with_an_order_to_fix_them() {
        // The point of this module: a fresh machine used to learn about these one run
        // at a time, and the second one arrived as a worker log tail.
        let root = PathBuf::from("/p");
        let out = report(&root, &[Gap::Project { root: root.clone() }, Gap::Provider]);
        let init = out.find("cowboy init").expect("init remedy");
        let setup = out.find("cowboy models setup").expect("setup remedy");
        assert!(init < setup, "project comes first:\n{out}");
        assert!(out.contains("in that order"), "{out}");
    }

    #[test]
    fn the_provider_gap_suppresses_a_redundant_model_gap() {
        // With no endpoint there is nothing to resolve against; two complaints for one
        // cause is how the old flow read.
        let tmp = assert_fs::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cowboy")).unwrap();
        std::fs::write(tmp.path().join(".cowboy/security.yaml"), "version: 1\n").unwrap();
        // Only meaningful when the host really has no provider; otherwise the check
        // legitimately passes and there is nothing to assert.
        if ProvidersConfig::load_global()
            .map(|p| p.providers.is_empty())
            .unwrap_or(true)
        {
            let gaps = check(tmp.path());
            assert_eq!(gaps, vec![Gap::Provider]);
        }
    }
}
