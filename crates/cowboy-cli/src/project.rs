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

/// Path to *this* cowboy binary, robust to the binary having been **replaced** since
/// the process started — `cargo install`, or a package upgrade, while a session runs.
///
/// On Linux `current_exe()` reads `/proc/self/exe`, which for a replaced executable
/// resolves to the literal string `".../cowboy (deleted)"`. That path does not exist, so
/// anything using it fails with `ENOENT`. The replacement almost always sits at the
/// same path, so strip the marker; failing that, look the bare name up on `PATH`.
///
/// This matters in two places that both looked unrelated to each other, which is why it
/// lives here rather than next to either of them:
///
/// - spawning a subagent, which fails to exec;
/// - **the sandbox's lockdown shim**, which the plan bind-mounts at
///   `cowboy_sandbox::SHIM_PATH`. That bind is rendered `--ro-bind-try`, so a missing
///   source is *silently skipped* — leaving nothing at the shim path and every command
///   in the session failing with `bwrap: execvp /.cowboy-shim: No such file or
///   directory`. The session stays alive and cannot run a single command.
pub fn self_exe() -> std::result::Result<PathBuf, String> {
    let raw = std::env::current_exe().map_err(|e| format!("cannot locate cowboy binary: {e}"))?;
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    resolve_exe(raw, &|p| p.exists(), &path_dirs)
        .ok_or_else(|| "cowboy binary not found (moved or upgraded mid-session?)".to_string())
}

/// Inner resolver, parameterized over existence + `PATH` for testing.
fn resolve_exe(
    raw: PathBuf,
    exists: &dyn Fn(&Path) -> bool,
    path_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if exists(&raw) {
        return Some(raw);
    }
    // A replaced executable's `/proc/self/exe` reads as `<path> (deleted)`.
    let s = raw.to_string_lossy();
    if let Some(stripped) = s.strip_suffix(" (deleted)") {
        let p = PathBuf::from(stripped);
        if exists(&p) {
            return Some(p);
        }
    }
    // Last resort: look up the bare binary name on PATH.
    let name = raw.file_name().map(|n| n.to_string_lossy().into_owned())?;
    let name = name.strip_suffix(" (deleted)").unwrap_or(&name).to_string();
    path_dirs.iter().map(|d| d.join(&name)).find(|c| exists(c))
}

/// A stable 32-bit hash of the project path, used to derive per-project network
/// names and subnets.
pub fn project_hash(root: &Path) -> u32 {
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    hasher.finish() as u32
}

/// The name of the scratch directory belonging to *this process's* sandbox for
/// `session_name`.
///
/// Owner-scoped rather than project-scoped, because a `NativeSandbox` always starts
/// its own holder and so its own namespaces: two sandboxes on one project are two
/// independent sessions. Sharing a directory between processes would leak one
/// session's `/tmp` into the other, and — worse — whichever stopped first would
/// delete the other's scratch out from under it. `cowboy sandbox exec` run while an
/// agent session is live is exactly that case.
///
/// Scoped to the process, not to each `NativeSandbox`: in production nothing runs two
/// sandboxes for one project in a single process (the worker has one, the one-off CLI
/// has one), and a pid is what makes an abandoned directory recognisable as such — see
/// [`ensure_scratch_dir`], which parses it back out. Two sandboxes in one process (the
/// integration tests) therefore do share this directory, which is survivable only
/// because `ensure_scratch_dir` runs per command and recreates what a sibling removed;
/// the cgroup could not be handled the same way, which is why [`cgroup_key`] is
/// per-instance.
pub fn scratch_key(session_name: &str) -> String {
    format!("{session_name}.{}", std::process::id())
}

/// The name of the cgroup belonging to *one* sandbox instance for `session_name`.
///
/// Instance-scoped, and deliberately more so than [`scratch_key`]. The cgroup was
/// originally named from the project alone, so every concurrent session in one
/// project — a foreman and its subagents, or `cowboy sandbox exec` run alongside a
/// live agent — shared a single directory. `Cgroup::create` reuses an existing
/// directory, so this was silent, and it broke in two ways:
///
/// - Whichever session tore down first ran `remove_dir` on the shared cgroup. That
///   succeeds whenever the directory momentarily holds no processes, which is true
///   of any sibling sitting idle between commands. Every surviving session then
///   failed to start its next command with `spawning the sandbox: No such file or
///   directory` — joining the cgroup is fatal on purpose — leaving an agent alive but
///   unable to run a single command for the rest of the session.
/// - The ceilings documented as per-session were really per-project, divided by
///   however many sessions happened to be live.
///
/// A pid is not enough on its own: one process may hold two sandboxes (integration
/// tests do), so a per-process counter distinguishes them. The `cowboy-` prefix that
/// [`crate::sandbox::cgroup::reap_empty`] matches on is added by `Cgroup::create`.
pub fn cgroup_key(session_name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{session_name}.{}.{seq}", std::process::id())
}

/// The session's scratch directory, created if absent, with the subdirectories the
/// plan binds at `/tmp`, `/run` and `/var/tmp`.
///
/// Host-side and outside the workspace: the agent reaches it only through those
/// mounts, so it cannot find it by path or notice it in the project.
///
/// `key` should come from [`scratch_key`]. Abandoned siblings are reaped here, which
/// is the only place guaranteed to run: a process killed with `SIGKILL` never gets to
/// clean up after itself, and scratch is disk-backed.
pub fn ensure_scratch_dir(key: &str) -> Result<PathBuf> {
    let base = private_dir()?.join("scratch");
    let dir = base.join(key);
    for (sub, _) in cowboy_sandbox::plan::SCRATCH_DIRS {
        let path = dir.join(sub);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating the sandbox scratch dir {}", path.display()))?;
    }
    ensure_mask_file(&dir)?;
    reap_abandoned_scratch(&base, key);
    Ok(dir)
}

/// The empty file bound over host-owned config to mask it, inside `dir`.
///
/// Deliberately per-session rather than one shared path under the cache directory.
/// The shared version was rewritten (fresh temp file, `rename` into place) by every
/// cowboy process that opened a sandbox, so a machine running several at once had many
/// readers of a name that others kept replacing — and sandbox startup failed
/// intermittently with an opaque `bwrap: Can't bind mount … mask-empty …: No such file
/// or directory`. Per-session, nothing else ever touches it.
///
/// It fails closed either way: a mask that cannot be bound stops the sandbox rather
/// than exposing `security.yaml` to the agent. That is why this was a startup failure
/// and never a boundary hole.
fn ensure_mask_file(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("mask-empty");
    // `create_new`, not `exists()` then `create`. The old form was a TOCTOU: two
    // commands in one session bringing a sandbox up at the same time both saw the file
    // absent, the first created it `0o400`, and the second's `write(true)` open then
    // failed with `EACCES` — on a file that was already exactly what it wanted. It only
    // showed up where tests run in parallel on a slow machine, which is to say in CI
    // and never on a dev box.
    //
    // O_EXCL closes the window, and "someone else created it" is success: the file is
    // empty by construction and never written, so any winner produces the same result.
    // Still created only-if-absent, for the original reason — a bind of it may be live
    // in another command of this session, and replacing it under them is the bug this
    // whole function exists to fix.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o400);
    }
    match opts.open(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("creating the config mask {}", path.display()))
        }
    }
    Ok(path)
}

/// Where the mask file lives inside a scratch directory. One definition, so the
/// plan and the thing that creates it cannot disagree.
pub fn mask_file_in(scratch: &Path) -> PathBuf {
    scratch.join("mask-empty")
}

/// Remove scratch directories whose owning process is gone. Best-effort throughout:
/// a directory we cannot classify is left alone rather than guessed at.
fn reap_abandoned_scratch(base: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == keep {
            continue;
        }
        // Only a name we minted, and only one whose owner is demonstrably gone.
        let Some((_, pid)) = name.rsplit_once('.') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// Delete a session's scratch directory. Best-effort: leftover scratch is untidy,
/// not dangerous, and refusing to tear a session down over it would be worse.
pub fn remove_scratch_dir(key: &str) {
    if let Ok(base) = private_dir() {
        let dir = base.join("scratch").join(key);
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %dir.display(), error = %e, "scratch not removed");
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The same project always yields the same session name — the daemon registry
    /// finds a session by it without asking a running worker.
    #[test]
    fn the_session_name_is_stable_per_project() {
        let a = session_name_for(Path::new("/tmp/one"));
        assert_eq!(a, session_name_for(Path::new("/tmp/one")));
        assert_ne!(a, session_name_for(Path::new("/tmp/two")));
        assert!(a.starts_with("cowboy-"), "{a}");
    }

    /// But the cgroup name must **not** be stable per project. It was, once, and two
    /// sessions in one project then shared a cgroup: the first to stop removed it and
    /// every sibling's next command died with `No such file or directory`.
    ///
    /// A pid alone is not enough — one process can hold two sandboxes — so the names
    /// must differ within a process too.
    #[test]
    fn cgroup_names_are_unique_per_instance() {
        let name = session_name_for(Path::new("/tmp/one"));
        let keys: Vec<_> = (0..4).map(|_| cgroup_key(&name)).collect();
        let unique: std::collections::BTreeSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "{keys:?}");
        for k in &keys {
            assert!(
                k.starts_with("cowboy-"),
                "the prefix cgroup::reap_empty matches on must survive: {k}"
            );
            assert!(k.contains(&format!(".{}.", std::process::id())), "{k}");
        }
    }
}

#[cfg(test)]
mod exe_tests {
    use super::*;

    /// Concurrent sandbox bring-ups must not fight over the config mask.
    ///
    /// `ensure_mask_file` used to check `exists()` and then open with `write(true)`. The
    /// file is created `0o400`, so the loser of that race opened an existing read-only
    /// file for writing and got `EACCES` — while the file was already exactly what it
    /// wanted. Two commands in one session, or a parallel test suite, is all it takes.
    /// It never fired on a fast dev box and failed two tests on the first CI run that
    /// got far enough to execute them.
    #[test]
    fn concurrent_sandbox_startups_share_one_config_mask() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();

        let results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|_| s.spawn(|| ensure_mask_file(&dir)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in &results {
            assert!(
                r.is_ok(),
                "every concurrent caller must get the mask, not EACCES: {:?}",
                r.as_ref().err()
            );
        }

        // And it really is the empty, read-only file the plan binds.
        let mask = mask_file_in(&dir);
        assert!(mask.is_file());
        assert_eq!(std::fs::metadata(&mask).unwrap().len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&mask).unwrap().permissions().mode() & 0o777,
                0o400,
                "the mask must stay read-only — it is bound over host-owned config"
            );
        }
    }

    #[test]
    fn resolve_exe_handles_a_replaced_binary() {
        let bin = PathBuf::from("/cargo/bin/cowboy");
        let deleted = PathBuf::from("/cargo/bin/cowboy (deleted)");

        // A live path is returned as-is.
        let exists_real = |p: &Path| p == bin;
        assert_eq!(
            resolve_exe(bin.clone(), &exists_real, &[]),
            Some(bin.clone())
        );

        // A `(deleted)` path resolves to the replacement at the same location.
        assert_eq!(resolve_exe(deleted.clone(), &exists_real, &[]), Some(bin));

        // If the same path is gone, fall back to the name on PATH.
        let path_dir = PathBuf::from("/usr/local/bin");
        let on_path = path_dir.join("cowboy");
        let exists_path = |p: &Path| p == on_path;
        assert_eq!(
            resolve_exe(deleted, &exists_path, &[path_dir]),
            Some(on_path)
        );
    }
}
