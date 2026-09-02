//! `cowboy sandbox` — inspect the confinement boundary.
//!
//! The boundary should be inspectable without reading the source or trusting a
//! summary of it. `plan` renders exactly what the next command would get: which
//! paths are readable and writable, what is masked, what the kernel-level
//! lockdown is, and which paths can never be granted at runtime.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cowboy_core::config::{expand_path, ConfigPaths, SecurityConfig};
use cowboy_sandbox::plan::{PlanInputs, SandboxPlan};
use cowboy_sandbox::{Denylist, HostProbe};

use crate::cli::{SandboxArgs, SandboxCommand};

pub fn run(args: SandboxArgs) -> Result<()> {
    match args.command {
        SandboxCommand::Plan => plan(),
    }
}

/// The real host, as seen by the plan builder.
pub struct RealHost;

impl HostProbe for RealHost {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn git_common_dir(&self, root: &Path) -> Option<PathBuf> {
        crate::net::runtime::git_common_dir(root)
    }

    fn expand(&self, raw: &str) -> Option<PathBuf> {
        expand_path(raw).ok()
    }

    fn home(&self) -> Option<PathBuf> {
        // Reuse config's expansion rather than adding a home-dir dependency, so
        // `~` resolves identically here and in every configured path.
        expand_path("~").ok()
    }

    fn self_exe(&self) -> Option<PathBuf> {
        std::env::current_exe().ok()
    }
}

fn plan() -> Result<()> {
    let root = crate::cmd::project_root()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let paths = ConfigPaths::for_root(&root);
    let mut security = SecurityConfig::load(&paths.security)
        .with_context(|| format!("loading {}", paths.security.display()))?;
    // Merge the personal overlay so the rendered plan matches what a session would
    // actually get — otherwise this would understate the boundary.
    cowboy_core::usersecrets::merge_into(&mut security, &crate::net::runtime::repo_key(&root));

    let probe = RealHost;
    // A representative mask path; the executor creates the real one per session.
    let mask = PathBuf::from("<mask: empty read-only file>");
    let inputs = PlanInputs {
        root: &root,
        security: &security,
        grants: &[],
        mask_file: &mask,
        relay_port: crate::sandbox::RELAY_PORT,
    };
    let plan = SandboxPlan::build(&inputs, &probe)?;
    let denylist = Denylist::build(&probe, &root);
    println!("project {}\n", root.display());
    print!("{}", plan.render(&denylist));
    Ok(())
}
