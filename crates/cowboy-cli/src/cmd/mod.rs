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
pub mod secrets;
pub mod session;
pub mod sessions;
pub mod skill;
pub mod worker;
pub mod worktree;

/// Locate the project root. For the MVP this is the current working directory;
/// later this may walk up to find an existing `.cowboy/` directory.
pub fn project_root() -> std::io::Result<std::path::PathBuf> {
    std::env::current_dir()
}
