//! Persisted path grants.
//!
//! A grant widens the sandbox's filesystem view: it is the answer to the most
//! common complaint about the container, which is that reaching a folder outside
//! the project meant editing config and rebuilding. Approve it once and the next
//! command sees it.
//!
//! SECURITY: grants are stored **host-side**, under `~/.config/cowboy/grants/`,
//! never inside the project. The workspace is mounted read-write into the sandbox,
//! so a grants file kept there would let a malicious model or repository widen its
//! own filesystem access by writing the file — the agent must never be able to grant
//! itself a path. This mirrors [`crate::net::approvals`], which keeps network
//! approvals out of the workspace for exactly the same reason.
//!
//! Being host-side is necessary but not sufficient. Every grant is checked against
//! the credential denylist when it is *used*, not only when it is written, because a
//! file can be hand-edited and a global grant outlives the project it was made in.
//! See [`crate::sandbox::native::NativeSandbox::plan`].

use std::path::{Path, PathBuf};

use cowboy_core::netproto::ApprovalScope;
use cowboy_sandbox::plan::Grant;

/// Where a grant is remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    /// This session only — never written down.
    Session,
    /// This project, in future sessions.
    Project,
    /// Every project on this machine.
    Global,
}

impl Persistence {
    /// Map a UI approval scope onto a grant's lifetime.
    ///
    /// `Once` becomes `Session`. A filesystem grant cannot meaningfully last for
    /// "one" of anything: it is a mount and a Landlock rule established when a
    /// command starts, so the smallest unit that can be expressed is a command, and
    /// a grant that vanished after one command would just make the agent ask again
    /// mid-task. Saying so here beats a surprising silent difference.
    pub fn from_scope(scope: ApprovalScope) -> Self {
        match scope {
            ApprovalScope::Once | ApprovalScope::Session => Persistence::Session,
            ApprovalScope::Project => Persistence::Project,
            ApprovalScope::Global => Persistence::Global,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Persistence::Session => "this session",
            Persistence::Project => "this project",
            Persistence::Global => "every project",
        }
    }
}

/// The host-only directory holding grants; falls back to the host temp dir when
/// there is no home config dir. Never inside the (agent-writable) workspace.
///
/// Callers hold this as a field rather than reaching for it per call, so a test can
/// point at a temp directory instead of the developer's real config — a unit test
/// that read the real `global.json` would behave differently on every machine.
pub fn dir() -> PathBuf {
    cowboy_core::config::global_config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("grants")
}

/// Grants file for one project, keyed by the root's hash.
fn project_file_in(dir: &Path, root: &Path) -> PathBuf {
    dir.join(format!(
        "{:08x}.json",
        crate::net::runtime::project_hash(root)
    ))
}

/// Grants that apply to every project on this machine.
fn global_file_in(dir: &Path) -> PathBuf {
    dir.join("global.json")
}

/// Persisted grants with the scope each came from, for `cowboy grant --list`.
pub fn listing(dir: &Path, root: &Path) -> Vec<(Grant, Persistence)> {
    read_file(&project_file_in(dir, root))
        .into_iter()
        .map(|g| (g, Persistence::Project))
        .chain(
            read_file(&global_file_in(dir))
                .into_iter()
                .map(|g| (g, Persistence::Global)),
        )
        .collect()
}

/// Every persisted grant that applies to `root`: the project's own, then the global
/// ones.
///
/// Project first so that a project grant naming the same path as a global one wins —
/// it is the more specific statement, and a global read-only grant must not silently
/// downgrade a project's read-write one.
pub fn load_in(dir: &Path, root: &Path) -> Vec<Grant> {
    let mut out = read_file(&project_file_in(dir, root));
    for g in read_file(&global_file_in(dir)) {
        if !out.iter().any(|existing| existing.path == g.path) {
            out.push(g);
        }
    }
    out
}

fn read_file(path: &Path) -> Vec<Grant> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Record a grant, returning whether anything changed.
pub fn add_in(
    dir: &Path,
    root: &Path,
    grant: &Grant,
    persistence: Persistence,
) -> std::io::Result<bool> {
    let file = match persistence {
        // Nothing to write: a session grant lives only in the sandbox's memory.
        Persistence::Session => return Ok(false),
        Persistence::Project => project_file_in(dir, root),
        Persistence::Global => global_file_in(dir),
    };
    let mut grants = read_file(&file);
    if let Some(existing) = grants.iter_mut().find(|g| g.path == grant.path) {
        if existing.read_only == grant.read_only {
            return Ok(false);
        }
        // Re-granting the same path with different access replaces it rather than
        // adding a second entry, so the effective access is never ambiguous.
        existing.read_only = grant.read_only;
    } else {
        grants.push(grant.clone());
    }
    write_file(dir, &file, &grants)?;
    Ok(true)
}

/// Forget a grant wherever it is recorded, returning whether anything changed.
pub fn remove_in(dir: &Path, root: &Path, path: &Path) -> std::io::Result<bool> {
    let mut changed = false;
    for file in [project_file_in(dir, root), global_file_in(dir)] {
        let mut grants = read_file(&file);
        let before = grants.len();
        grants.retain(|g| g.path != path);
        if grants.len() != before {
            write_file(dir, &file, &grants)?;
            changed = true;
        }
    }
    Ok(changed)
}

fn write_file(dir: &Path, file: &Path, grants: &[Grant]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(file, serde_json::to_string_pretty(grants)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(path: &str, read_only: bool) -> Grant {
        Grant {
            path: PathBuf::from(path),
            read_only,
        }
    }

    #[test]
    fn a_project_grant_round_trips() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        let root = Path::new("/srv/project");

        assert!(load_in(&dir, root).is_empty());
        assert!(add_in(&dir, root, &grant("/data", false), Persistence::Project).unwrap());
        assert_eq!(load_in(&dir, root), vec![grant("/data", false)]);
        // Adding the same grant again changes nothing.
        assert!(!add_in(&dir, root, &grant("/data", false), Persistence::Project).unwrap());
    }

    /// Grants are per-project, so one project's grant must not appear in another's.
    #[test]
    fn a_project_grant_does_not_leak_to_another_project() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        add_in(
            &dir,
            Path::new("/srv/a"),
            &grant("/data", false),
            Persistence::Project,
        )
        .unwrap();
        assert!(load_in(&dir, Path::new("/srv/b")).is_empty());
    }

    #[test]
    fn a_global_grant_applies_to_every_project() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        add_in(
            &dir,
            Path::new("/srv/a"),
            &grant("/opt/toolchain", true),
            Persistence::Global,
        )
        .unwrap();
        assert_eq!(
            load_in(&dir, Path::new("/srv/b")),
            vec![grant("/opt/toolchain", true)]
        );
    }

    /// Re-granting a path with different access must *replace* it, not leave two
    /// entries whose effective access depends on iteration order.
    #[test]
    fn regranting_a_path_replaces_its_access() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        let root = Path::new("/srv/project");
        add_in(&dir, root, &grant("/data", true), Persistence::Project).unwrap();
        assert!(add_in(&dir, root, &grant("/data", false), Persistence::Project).unwrap());
        assert_eq!(load_in(&dir, root), vec![grant("/data", false)]);
    }

    /// A project grant for the same path as a global one wins: it is the more
    /// specific statement, and a global read-only grant must not silently downgrade
    /// a project's read-write one.
    #[test]
    fn a_project_grant_wins_over_a_global_one_for_the_same_path() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        let root = Path::new("/srv/project");
        add_in(&dir, root, &grant("/data", true), Persistence::Global).unwrap();
        add_in(&dir, root, &grant("/data", false), Persistence::Project).unwrap();
        assert_eq!(load_in(&dir, root), vec![grant("/data", false)]);
    }

    #[test]
    fn remove_forgets_a_grant_at_either_scope() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        let root = Path::new("/srv/project");
        add_in(&dir, root, &grant("/data", false), Persistence::Project).unwrap();
        add_in(&dir, root, &grant("/opt/x", true), Persistence::Global).unwrap();

        assert!(remove_in(&dir, root, Path::new("/data")).unwrap());
        assert!(remove_in(&dir, root, Path::new("/opt/x")).unwrap());
        assert!(load_in(&dir, root).is_empty());
        // Removing something that was never granted is not an error, just no change.
        assert!(!remove_in(&dir, root, Path::new("/nope")).unwrap());
    }

    #[test]
    fn a_session_grant_is_never_written_down() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        let root = Path::new("/srv/project");
        assert!(!add_in(&dir, root, &grant("/data", false), Persistence::Session).unwrap());
        assert!(load_in(&dir, root).is_empty());
        assert!(!dir.exists(), "a session grant must not create a file");
    }

    /// A corrupt or hand-mangled file must not stop the sandbox starting. Losing a
    /// grant is recoverable (re-grant it); refusing to run is not.
    #[test]
    fn a_corrupt_file_reads_as_no_grants() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(project_file_in(&dir, Path::new("/srv/p")), "{not json").unwrap();
        assert!(load_in(&dir, Path::new("/srv/p")).is_empty());
    }

    #[test]
    fn once_and_session_are_the_same_lifetime() {
        assert_eq!(
            Persistence::from_scope(ApprovalScope::Once),
            Persistence::Session
        );
        assert_eq!(
            Persistence::from_scope(ApprovalScope::Session),
            Persistence::Session
        );
        assert_eq!(
            Persistence::from_scope(ApprovalScope::Project),
            Persistence::Project
        );
        assert_eq!(
            Persistence::from_scope(ApprovalScope::Global),
            Persistence::Global
        );
    }
}
