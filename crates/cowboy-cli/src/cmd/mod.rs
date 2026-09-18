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
    let root = crate::project::resolve_root(&std::env::current_dir()?);
    // Refuse a root that cannot be represented on the wire, here, once, with a sentence
    // that names the problem.
    //
    // `PathBuf` is not required to be UTF-8 on Linux, but the daemon protocol is JSON and
    // serde refuses to serialize a non-UTF-8 path — so such a root would travel fine
    // through every host-side call and then fail deep inside a socket writer, where the
    // best available outcome is a silently dropped message. Cowboy cannot support a
    // project at this path, and saying so at the boundary beats degrading in the middle.
    if root.to_str().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "project path is not valid UTF-8 ({}) — cowboy cannot address it over the \
                 daemon protocol; rename the directory",
                root.display()
            ),
        ));
    }
    Ok(root)
}
