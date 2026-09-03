//! Unix sockets that only their owner may talk to.
//!
//! Cowboy's two control surfaces are unix sockets under `$XDG_RUNTIME_DIR/cowboy`:
//! the daemon's `cowboyd.sock` and one `s-<id>.sock` per live session. Both are
//! **fully privileged interfaces**. Over the daemon socket a client starts and ends
//! sessions, creates worktrees and drives workers; over a session socket it injects
//! messages into the agent's conversation and — the part that matters most — answers
//! outstanding network-approval prompts. A peer who can answer those *is* the `ask`
//! policy gate.
//!
//! Neither used to be protected. `UnixListener::bind` applies the process umask, so
//! the socket landed `0755` on a typical host, and the directory was created with
//! `create_dir_all`'s default. On a systemd host `$XDG_RUNTIME_DIR` is itself `0700`,
//! which hid the problem; the fallback path (`/tmp/cowboy-<uid>`, used whenever
//! `XDG_RUNTIME_DIR` is unset — a plain `ssh` session, a container, a cron job) put a
//! world-connectable socket in a world-writable directory. Any local user could
//! approve their own egress.
//!
//! Three layers here, because each covers a different failure:
//!
//! 1. **The directory is `0700` and verified to be ours** — not a symlink, not
//!    another user's. This is the load-bearing one: it is what stops a hostile
//!    pre-created path, which no permission on the socket itself could fix, and it
//!    means no other user can reach the socket path at all regardless of the mode on
//!    the socket.
//! 2. **The socket is chmod'ed `0600`**, so the mode agrees with the intent even if
//!    the directory is later loosened by hand. Done after the bind rather than by
//!    narrowing the umask across it: the umask is process-global, and an earlier
//!    version of this narrowed it while other threads were creating directories, one
//!    of which landed `0000` and could not be used. The window a chmod leaves is
//!    inside a directory nobody else can traverse, so there is nothing to race.
//! 3. **Every accepted connection's peer uid is checked** with `SO_PEERCRED` and
//!    dropped unless it is ours. Permissions are advisory in the sense that anything
//!    which touches the file can undo them; the kernel's answer to "who is on the
//!    other end" cannot be.
//!
//! Note that `SO_PEERCRED` is deliberately *not* treated as a way to let root in:
//! root can already do anything, so admitting uid 0 buys nothing and would make a
//! root-run client silently work where the same client run as the user would not.

use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::net::{UnixListener, UnixStream};

/// Our uid. Cheap enough to call per connection.
fn our_uid() -> u32 {
    // SAFETY: getuid() takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// Create `dir` as an owner-only directory, refusing anything we do not own.
///
/// Fatal rather than best-effort: the sockets inside it are a control surface, and
/// carrying on with a directory someone else can write to would put one there.
pub fn ensure_private_dir(dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    // Created 0700 from the start, so there is no window at the default mode.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;

    // `symlink_metadata`, not `metadata`: the point is to notice a symlink rather
    // than follow it to whatever it names.
    let md =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;
    if md.file_type().is_symlink() || !md.is_dir() {
        anyhow::bail!(
            "{} is not a real directory; refusing to put a control socket there",
            dir.display()
        );
    }
    if md.uid() != our_uid() {
        anyhow::bail!(
            "{} is owned by uid {}, not by you; refusing to put a control socket there",
            dir.display(),
            md.uid()
        );
    }
    // `recursive(true)` does not apply `mode` to a directory that already existed,
    // and one created by an older cowboy is 0755. `mkdir` also masks the requested
    // mode with the umask, which another thread may have narrowed. Either way, set
    // it exactly rather than trusting what landed.
    if md.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {} to owner-only", dir.display()))?;
    }
    Ok(dir.to_path_buf())
}

/// Bind a listening unix socket at `path`, reachable only by this user.
///
/// The parent directory is created and verified first — that is what makes the socket
/// unreachable by anyone else — and then the socket is chmod'ed `0600`. Any stale
/// socket at `path` is removed; callers hold whatever lock makes that safe.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;

    // Set exactly rather than clearing group/other bits: `0000` is as broken as
    // `0755` here, since connecting to a socket needs write permission on it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {} to owner-only", path.display()))?;
    }
    Ok(listener)
}

/// Refuse a socket that is not ours before saying anything to it.
///
/// The mirror of the accept-side check, and it matters for the same reason: a client
/// hands the thing on the other end its project paths, its tasks, and its answers to
/// approval prompts. On a host with no `XDG_RUNTIME_DIR` the socket directory is a
/// predictable path in world-writable `/tmp`, so before this existed a local user
/// could pre-create it and stand up a fake `cowboyd` for the real user to talk to.
fn verify_ours(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    // `symlink_metadata`: a symlink here would point the check at one file and the
    // connect at another.
    let md = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {}", path.display()))?;
    if !md.file_type().is_socket() {
        anyhow::bail!("{} is not a socket; refusing to talk to it", path.display());
    }
    if md.uid() != our_uid() {
        anyhow::bail!(
            "{} is owned by uid {}, not by you; refusing to talk to it",
            path.display(),
            md.uid()
        );
    }
    Ok(())
}

/// Connect to a unix socket, refusing one that is not owned by this user.
pub async fn connect(path: &Path) -> Result<UnixStream> {
    verify_ours(path)?;
    UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to {}", path.display()))
}

/// The blocking equivalent, for a caller that only probes liveness.
pub fn connect_blocking(path: &Path) -> Result<std::os::unix::net::UnixStream> {
    verify_ours(path)?;
    std::os::unix::net::UnixStream::connect(path)
        .with_context(|| format!("connecting to {}", path.display()))
}

/// The uid on the other end of `stream`, from the kernel rather than from anything
/// the peer said.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a correctly sized, owned buffer for SO_PEERCRED on an
    // `AF_UNIX` socket, and `len` describes it.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Whether `stream`'s peer is this user, logging and rejecting anything else.
///
/// Fails closed: a peer whose credentials cannot be read is refused, since an
/// unidentifiable caller on a control socket is exactly what this exists to stop.
pub fn peer_is_ours(stream: &UnixStream) -> bool {
    match peer_uid(stream) {
        Ok(uid) if uid == our_uid() => true,
        Ok(uid) => {
            tracing::warn!(
                peer_uid = uid,
                our_uid = our_uid(),
                "refused a control-socket connection from another user"
            );
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "refused a control-socket connection with unreadable credentials");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn a_bound_socket_is_owner_only_and_so_is_its_directory() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("run");
        let path = dir.join("t.sock");
        let _listener = bind(&path).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "a control socket must not be connectable by other users"
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "nor may its directory let anyone else create or replace entries"
        );
    }

    /// A directory left `0755` by an older version is tightened rather than accepted:
    /// upgrading must close the hole, not require the user to notice it.
    #[tokio::test]
    async fn an_existing_world_readable_directory_is_tightened() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let dir = tmp.path().join("run");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _listener = bind(&dir.join("t.sock")).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// A symlink where the socket directory should be is refused outright. Following
    /// it would create a socket somewhere its planter chose.
    #[test]
    fn a_symlinked_socket_directory_is_refused() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = tmp.path().join("run");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = ensure_private_dir(&link).expect_err("a symlinked directory must be refused");
        assert!(err.to_string().contains("not a real directory"), "{err}");
    }

    /// Our own connections must be admitted — a check that rejected everything would
    /// pass the security test and break the product.
    #[tokio::test]
    async fn our_own_connection_is_admitted() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("run/t.sock");
        let listener = bind(&path).unwrap();
        let client = tokio::spawn(async move { UnixStream::connect(&path).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        let _client = client.await.unwrap();

        assert_eq!(peer_uid(&server).unwrap(), our_uid());
        assert!(peer_is_ours(&server));
    }
}
