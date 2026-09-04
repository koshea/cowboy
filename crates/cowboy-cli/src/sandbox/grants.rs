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
        .unwrap_or_else(|| {
            // A predictable name in world-writable `/tmp` was the old fallback, for a
            // file that lists **host paths mounted into the sandbox**. Anyone able to
            // pre-create it could hand the agent a directory nobody granted. uid-scoped
            // now, and verified owner-only at every read and write below.
            // SAFETY: getuid() takes no arguments and cannot fail.
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("cowboy-{uid}"))
        })
        .join("grants")
}

/// Verify the store directory is a real directory owned by us with no access for anyone
/// else, creating it if needed.
///
/// Refusing beats falling back: a grants file another user can write is a list of paths
/// they choose to expose to the agent.
fn store_dir(dir: &Path) -> std::io::Result<()> {
    crate::localsock::ensure_private_dir(dir)
        .map(|_| ())
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Whether persisted grants under `dir` may be trusted.
///
/// Stricter than the write path deliberately. `store_dir` *tightens* a loose directory we
/// own, which is right when we are about to write — from that moment it is private. But
/// for reading, a directory that is currently group- or world-**writable** means someone
/// else could already have planted entries, and tightening the mode does not un-plant
/// them. The contents are suspect, so they are ignored rather than trusted and fixed.
fn readable(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !dir.exists() {
        return true; // nothing persisted yet
    }
    let Ok(md) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    let refuse = |why: &str| {
        tracing::warn!(path = %dir.display(), why, "ignoring persisted grants");
        false
    };
    if md.file_type().is_symlink() || !md.is_dir() {
        return refuse("not a real directory");
    }
    // SAFETY: getuid() takes no arguments and cannot fail.
    if md.uid() != unsafe { libc::getuid() } {
        return refuse("owned by another user");
    }
    if md.permissions().mode() & 0o022 != 0 {
        return refuse("writable by others, so its entries may not be ours");
    }
    true
}

/// Grants file for one project, keyed by the root's hash.
fn project_file_in(dir: &Path, root: &Path) -> PathBuf {
    dir.join(format!("{:08x}.json", crate::project::project_hash(root)))
}

/// Grants that apply to every project on this machine.
fn global_file_in(dir: &Path) -> PathBuf {
    dir.join("global.json")
}

/// Persisted grants with the scope each came from, for `cowboy grant --list`.
pub fn listing(dir: &Path, root: &Path) -> Vec<(Grant, Persistence)> {
    if !readable(dir) {
        return Vec::new();
    }
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
    if !readable(dir) {
        return Vec::new();
    }
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

/// Write the grants file owner-only.
///
/// A grant is a host path the sandbox may see. The list of them is a map of which of
/// the user's directories a sandbox has been let into — worth no more exposure than
/// the `providers.yaml` it sits beside, and `fs::write` created it at the process
/// umask, i.e. world-readable on a typical host.
///
/// The mode is set on the `open`, so the file is never briefly readable, and re-applied
/// afterwards because an existing file keeps its inode (and so an older version's
/// mode). The directory is created `0700` for the same reason: a grants file is only as
/// private as the directory holding it.
fn write_file(dir: &Path, file: &Path, grants: &[Grant]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // Creates it 0700 and refuses a directory owned by anyone else.
    store_dir(dir)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?;
    f.write_all(serde_json::to_string_pretty(grants)?.as_bytes())?;
    f.sync_all()?;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
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

    /// A grants store we cannot verify as our own must yield nothing, and refuse writes.
    ///
    /// The path used to fall back to bare `env::temp_dir()`, i.e. a predictable name in a
    /// world-writable directory — for a file listing host paths mounted into the sandbox.
    /// Anyone able to pre-create it could hand the agent a directory nobody granted.
    /// Verified at use rather than only by choosing a better path, which also protects
    /// the ordinary `~/.config` location against a mis-permissioned home.
    #[test]
    fn a_grants_directory_that_is_not_ours_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        let root = Path::new("/srv/project");

        // Seed a legitimate grant, then loosen the directory as a hostile /tmp would be.
        add_in(&dir, root, &grant("/data", false), Persistence::Project).unwrap();
        assert_eq!(load_in(&dir, root), vec![grant("/data", false)]);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        assert!(
            load_in(&dir, root).is_empty(),
            "grants from a world-writable directory must not be honoured"
        );
        assert!(
            listing(&dir, root).is_empty(),
            "nor listed as though they applied"
        );
        // A write still succeeds and tightens the directory: we own it, and from that
        // moment it is private. Only *trusting existing entries* is refused, because
        // fixing the mode cannot un-plant what was already there.
        add_in(&dir, root, &grant("/other", true), Persistence::Project).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Tightened, so the store is trusted again.
        assert!(!load_in(&dir, root).is_empty());
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

    /// A grants file is a list of the user's directories a sandbox has been let into.
    /// It sits in the same config dir as `providers.yaml` and must be no looser —
    /// `fs::write` created it at the process umask, world-readable on a typical host.
    #[test]
    fn grants_are_not_readable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("grants");
        let root = Path::new("/srv/project");
        add_in(&dir, root, &grant("/data", false), Persistence::Project).unwrap();
        add_in(&dir, root, &grant("/other", true), Persistence::Global).unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&project_file_in(&dir, root)), 0o600);
        assert_eq!(mode(&global_file_in(&dir)), 0o600);
        assert_eq!(mode(&dir), 0o700, "nor may the directory be traversable");
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
