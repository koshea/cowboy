//! Project identity and host-side helpers.
//!
//! What survived the container runtime. These are the pieces that were never about
//! Docker: how a project is named and keyed, how its git directory is found, and how
//! a private host-only file is written.
//!
//! Kept together rather than scattered because they share one property — every one
//! of them runs **host-side**, outside the sandbox, on paths the agent cannot reach.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cowboy_core::config;

/// A stable 32-bit hash of the project path, used to derive per-project network
/// names and subnets.
pub fn project_hash(root: &Path) -> u32 {
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    hasher.finish() as u32
}

/// The deterministic name of this project's sandbox session.
///
/// One definition, used by the sandbox itself and by the daemon registry, so a
/// session can be identified without asking a running worker.
pub fn session_name_for(root: &Path) -> String {
    format!("cowboy-{:08x}", project_hash(root))
}

/// Run a host command and return its trimmed stdout as a secret value, or
/// `None` if it fails / produces nothing / exceeds the timeout. Used for
/// keyring-backed tokens (`gh auth token`). The command comes from host-owned
/// config; never logged.
///
/// stdin is `/dev/null` so a credential helper that would otherwise prompt
/// interactively fails fast instead of blocking, and a bounded timeout backstops
/// anything that still hangs — this runs (cached) on every shell exec, so a hang
/// here would otherwise deadlock the whole session.
fn run_value_command(cmd: &str) -> Option<String> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let cmd = cmd.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    let out = rx.recv_timeout(TIMEOUT).ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Whether a `source_command` produces a value on the host, using the same
/// bounded/`stdin`-null execution as the live path (so `cowboy secrets list`
/// can't hang on an interactive credential helper). Does not expose the value.
pub(crate) fn source_command_ok(cmd: &str) -> bool {
    run_value_command(cmd).is_some()
}

/// The repository root that's shared by every worktree: `git rev-parse
/// --git-common-dir` resolves to `<main-repo>/.git` from both a normal checkout
/// and a linked worktree, so its parent is the one repo they share. Falls back
/// to `root` for a non-git directory.
pub fn repo_root(root: &Path) -> PathBuf {
    if let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
    {
        if out.status.success() {
            let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
            if let Some(parent) = common.parent() {
                if !parent.as_os_str().is_empty() {
                    return parent.to_path_buf();
                }
            }
        }
    }
    root.to_path_buf()
}

/// The per-repository overlay key (stable across all of a repo's worktrees).
/// Used for the personal credential overlay so a grant applies to every worktree.
pub fn repo_key(root: &Path) -> String {
    format!("{:08x}", project_hash(&repo_root(root)))
}

/// The shared git directory to mount when `root` is a *linked worktree* — i.e.
/// `<root>/.git` is a file (a `gitdir:` pointer into the main repo) rather than
/// a directory. Returns the main repo's git common dir (e.g. `<main>/.git`),
/// which lives outside `<root>` and must be mounted at its own absolute path so
/// the worktree's gitdir reference resolves in the container. `None` for a
/// normal repo (its `.git` dir is already inside the workspace mount) or a
/// non-git directory.
pub(crate) fn git_common_dir(root: &Path) -> Option<PathBuf> {
    // Only linked worktrees have a `.git` *file*; a normal repo has a directory
    // that's already covered by the /workspace mount.
    if !root.join(".git").is_file() {
        return None;
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    // It lives outside the workspace by definition; guard anyway, and require
    // that it actually exists on the host.
    if dir.as_os_str().is_empty() || dir.starts_with(root) || !dir.exists() {
        return None;
    }
    Some(dir)
}

/// A private (`0700`), user-owned directory for cowboy's runtime artifacts — the
/// config mask file, the gateway's policy file.
///
/// These used to live at predictable paths in world-writable `/tmp`, where a local
/// user could pre-create them. That mattered: the mask file is bind-mounted into
/// the container, so a symlink there would make dockerd (root) mount *any* file the
/// cowboy user can read — `providers.yaml` included — into the untrusted agent; and
/// the policy file is the allow/deny list the gateway enforces. Owning the
/// directory removes the class: no other user can create or replace entries in it.
pub(crate) fn private_dir() -> Result<PathBuf> {
    let dir = config::global_cache_dir()
        .context("cannot resolve a cache directory for cowboy's runtime files")?
        .join("run");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Not a symlink someone else planted, and ours.
        let md = std::fs::symlink_metadata(&dir)
            .with_context(|| format!("inspecting {}", dir.display()))?;
        if md.file_type().is_symlink() || !md.is_dir() {
            anyhow::bail!(
                "{} is not a real directory; refusing to use it",
                dir.display()
            );
        }
        // SAFETY: getuid() is always safe.
        if md.uid() != unsafe { libc::getuid() } {
            anyhow::bail!(
                "{} is not owned by this user; refusing to use it",
                dir.display()
            );
        }
        // Fatal, not best-effort: the whole point is that others can't write here.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {} to owner-only", dir.display()))?;
    }
    Ok(dir)
}

/// Create a file inside [`private_dir`] with `contents`, replacing any existing
/// one. Fails rather than following a symlink or reusing a file we don't own.
pub(crate) fn write_private_file(name: &str, contents: &[u8]) -> Result<PathBuf> {
    let dir = private_dir()?;
    let path = dir.join(name);
    // Write to a unique temporary name and rename into place.
    //
    // Never open the destination directly: a pre-existing symlink there would make
    // us write *through* it to a file of someone else's choosing. Creating a fresh
    // name exclusively and renaming avoids that, and unlike `remove` + `create_new`
    // it is atomic and safe when two callers race — that pattern fails the loser
    // with `AlreadyExists`, which showed up as a flaky test once a second caller
    // existed.
    let tmp = dir.join(format!("{name}.{}.{}", std::process::id(), now_nanos()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    use std::io::Write;
    let write = f
        .write_all(contents)
        .with_context(|| format!("writing {}", tmp.display()));
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
        write?;
    }
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("installing {}", path.display()));
    }
    Ok(path)
}

/// Nanoseconds since the epoch, for a unique temporary filename.
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The empty file used to mask host-owned config inside the container.
pub(crate) fn ensure_mask_file() -> Result<PathBuf> {
    write_private_file("mask-empty", b"")
}
