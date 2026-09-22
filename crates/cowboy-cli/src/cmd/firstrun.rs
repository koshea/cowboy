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
use cowboy_core::config::{
    resolve_model, AgentConfig, ConfigPaths, ModelsConfig, ProvidersConfig, SecurityConfig,
};
use cowboy_core::Error;

use crate::style;

/// Something that prevents a session from starting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gap {
    /// No required `security.yaml` for this project.
    Project { root: PathBuf },
    /// A present config file could not be read, parsed, or validated.
    InvalidConfig {
        name: String,
        path: PathBuf,
        detail: String,
    },
    /// No provider in the home config (endpoint + key).
    Provider,
    /// A provider exists, but no usable model.
    Model { detail: String },
}

impl Gap {
    /// What is wrong, and the action that fixes it.
    fn line(&self) -> (String, String) {
        match self {
            Gap::Project { root } => (
                format!("no .cowboy/ config in {}", root.display()),
                "cowboy init".into(),
            ),
            Gap::InvalidConfig { name, path, detail } => (
                format!("invalid {name}: {detail}"),
                format!("fix {}", path.display()),
            ),
            Gap::Provider => (
                "no model provider configured".into(),
                "cowboy models setup".into(),
            ),
            Gap::Model { detail } => (detail.clone(), "cowboy models add <model-id>".into()),
        }
    }
}

#[derive(Debug)]
enum Loaded<T> {
    Absent,
    Value(T),
    Invalid(String),
}

fn loaded<T>(result: cowboy_core::Result<T>) -> Loaded<T> {
    match result {
        Ok(value) => Loaded::Value(value),
        Err(Error::ConfigNotFound(_)) => Loaded::Absent,
        Err(error) => Loaded::Invalid(error.to_string()),
    }
}

fn loaded_opt<T>(result: cowboy_core::Result<Option<T>>) -> Loaded<T> {
    match result {
        Ok(Some(value)) => Loaded::Value(value),
        Ok(None) | Err(Error::ConfigNotFound(_)) => Loaded::Absent,
        Err(error) => Loaded::Invalid(error.to_string()),
    }
}

fn record_config<T>(gaps: &mut Vec<Gap>, name: &str, path: &Path, state: Loaded<T>) -> Option<T> {
    match state {
        Loaded::Value(value) => Some(value),
        Loaded::Absent => None,
        Loaded::Invalid(detail) => {
            gaps.push(Gap::InvalidConfig {
                name: name.into(),
                path: path.to_path_buf(),
                detail,
            });
            None
        }
    }
}

/// Everything a session start needs, in the order a user would fix it.
///
/// Every existing file is loaded strictly and independently. A malformed file is never
/// collapsed into "missing": doing so can recommend `init` or `models setup`, both of
/// which are the wrong operation when the user's file needs repair.
pub fn check(root: &Path) -> Vec<Gap> {
    let paths = ConfigPaths::for_root(root);
    let providers_path = ProvidersConfig::global_path();
    let user_models_path = ModelsConfig::user_path();
    let mut gaps = Vec::new();

    match loaded(SecurityConfig::load(&paths.security)) {
        Loaded::Absent => gaps.push(Gap::Project {
            root: root.to_path_buf(),
        }),
        state => {
            record_config(&mut gaps, "security.yaml", &paths.security, state);
        }
    }
    record_config(
        &mut gaps,
        "agent.yaml",
        &paths.agent,
        loaded_opt(AgentConfig::load_opt(&paths.agent)),
    );

    let providers = match &providers_path {
        Some(path) => record_config(
            &mut gaps,
            "providers.yaml",
            path,
            loaded(ProvidersConfig::load_global()),
        ),
        None => {
            gaps.push(Gap::InvalidConfig {
                name: "providers.yaml".into(),
                path: PathBuf::from("~/.config/cowboy/providers.yaml"),
                detail: "cannot resolve the home config directory".into(),
            });
            None
        }
    };
    let user = match &user_models_path {
        Some(path) => record_config(
            &mut gaps,
            "user models.yaml",
            path,
            loaded_opt(ModelsConfig::load_opt(path)),
        ),
        None => None,
    };
    let project = record_config(
        &mut gaps,
        "project models.yaml",
        &paths.models,
        loaded_opt(ModelsConfig::load_opt(&paths.models)),
    );

    if let Some(providers) = providers {
        if providers.providers.is_empty() {
            gaps.push(Gap::Provider);
        } else if let Err(error) = resolve_model(&providers, user.as_ref(), project.as_ref(), None)
        {
            gaps.push(Gap::Model {
                detail: format!("a provider is configured, but no usable model ({error})"),
            });
        }
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
    fn invalid_config_is_never_reported_as_absent_or_sent_to_setup() {
        let root = PathBuf::from("/p");
        let path = root.join(".cowboy/models.yaml");
        let gaps = [Gap::InvalidConfig {
            name: "project models.yaml".into(),
            path: path.clone(),
            detail: "failed to parse".into(),
        }];
        let out = report(&root, &gaps);
        assert!(out.contains("invalid project models.yaml"), "{out}");
        assert!(out.contains(&format!("fix {}", path.display())), "{out}");
        assert!(!out.contains("cowboy init"), "{out}");
        assert!(!out.contains("cowboy models setup"), "{out}");
    }

    #[test]
    fn independent_invalid_configs_are_all_recorded() {
        let root = PathBuf::from("/p");
        let mut gaps = Vec::new();
        let first: Loaded<()> = Loaded::Invalid("bad yaml".into());
        let second: Loaded<()> = Loaded::Invalid("unknown field".into());
        record_config(
            &mut gaps,
            "providers.yaml",
            Path::new("/home/providers.yaml"),
            first,
        );
        record_config(
            &mut gaps,
            "project models.yaml",
            &root.join("models.yaml"),
            second,
        );
        assert_eq!(gaps.len(), 2);
        assert!(gaps
            .iter()
            .all(|gap| matches!(gap, Gap::InvalidConfig { .. })));
    }
}
