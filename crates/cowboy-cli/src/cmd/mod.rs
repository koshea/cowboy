//! Subcommand implementations.

pub mod attach;
pub mod daemon;
pub mod doctor;
pub mod down;
pub mod fileop;
pub mod init;
pub mod logs;
pub mod models;
pub mod patch;
pub mod proc;
pub mod run;
pub mod session;
pub mod skill;
pub mod worker;

/// Locate the project root. For the MVP this is the current working directory;
/// later this may walk up to find an existing `.cowboy/` directory.
pub fn project_root() -> std::io::Result<std::path::PathBuf> {
    std::env::current_dir()
}
