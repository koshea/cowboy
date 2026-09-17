//! Subcommand implementations.

pub mod agents;
pub mod artifact;
pub mod attach;
pub mod bus;
pub mod crew;
pub mod daemon;
pub mod decisions;
pub mod doctor;
pub mod down;
pub mod fileop;
pub mod firstrun;
pub mod grant;
pub mod handoff;
pub mod init;
pub mod logs;
pub mod mcp;
pub mod memory;
pub mod models;
pub mod patch;
pub mod proc;
pub mod ranch;
pub mod review;
pub mod run;
pub mod sandbox;
pub mod secrets;
pub mod session;
pub mod sessions;
pub mod skill;
pub mod web;
pub mod worker;
pub mod worktree;

/// Locate the project root: the nearest ancestor holding a `.cowboy/` directory,
/// else the enclosing git worktree, else the current directory.
///
/// The cwd alone is not the project. Running `cowboy` from `crates/foo/` in an
/// initialized repo used to behave exactly like running it in an uninitialized
/// directory — no config found, a different sandbox mount, a different session
/// directory, and a worktree lease that failed to collide with the session already
/// running at the root. Every tool a developer already has in their hands (`git`,
/// `cargo`, `npm`) resolves its project by walking up; this now does too.
pub fn project_root() -> std::io::Result<std::path::PathBuf> {
    Ok(crate::project::resolve_root(&std::env::current_dir()?))
}
