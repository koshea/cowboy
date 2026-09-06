//! Safe filesystem writes for stores that live in the agent-writable workspace.
//!
//! The Ranch store (`.cowboy/ranches/...`) is inside the project, which is
//! bind-mounted **writable** into the sandbox. Its writers, however, run
//! **host-side** in the worker process (the agent loop is not sandboxed — only its
//! shell commands are). A plain `std::fs::write(tmp, ..)` opens with
//! `O_CREAT|O_WRONLY|O_TRUNC` and **follows symlinks**, so sandboxed code can plant
//! a symlink at the predictable tmp path and redirect the host-side write to an
//! arbitrary file it could not otherwise touch (e.g. `~/.config/cowboy/*`). That is
//! a confused-deputy TOCTOU — the same class the sandbox mask file closed with
//! `O_EXCL` (`ensure_mask_file`).
//!
//! [`write_atomic`] closes it: the temp file is created with `O_EXCL|O_NOFOLLOW`
//! (via `create_new` plus `custom_flags(O_NOFOLLOW)`), so a pre-existing file *or* a
//! symlink at the tmp path is refused rather than followed, and the final content is
//! moved into place with a rename.

use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

/// Atomically write `contents` to `path`, refusing to follow a symlink at either
/// the temporary file or (via the exclusive create) clobber a planted regular file.
///
/// Writes to `<path>.tmp` created with `O_CREAT|O_EXCL|O_NOFOLLOW`, then renames it
/// over `path`. A stale `<path>.tmp` (e.g. from a previous crash) is removed first
/// so the exclusive create can succeed — but only via `remove_file`, which does not
/// follow a final-component symlink, so a planted symlink is unlinked, not chased.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Invalid(e.to_string()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));

    // Remove any stale/planted tmp entry. `remove_file` unlinks the name without
    // following it, so a symlink here is deleted rather than traversed.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Invalid(format!("clearing {}: {e}", tmp.display()))),
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true); // O_CREAT|O_EXCL: refuses an existing file/symlink
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW: refuse even if the final component is a symlink (belt-and-
        // suspenders alongside O_EXCL, and correct if the remove above raced).
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts
        .open(&tmp)
        .map_err(|e| Error::Invalid(format!("creating {}: {e}", tmp.display())))?;
    f.write_all(contents)
        .map_err(|e| Error::Invalid(format!("writing {}: {e}", tmp.display())))?;
    f.sync_all().ok();
    drop(f);

    std::fs::rename(&tmp, path).map_err(|e| Error::Invalid(format!("renaming into place: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces_atomically() {
        let dir = std::env::temp_dir().join(format!("cowboy-fs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.yaml");
        write_atomic(&path, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        // Overwrites cleanly on a second call (stale tmp handling works).
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A symlink planted at the tmp path must NOT be followed — the write must land
    /// on the real target (via the refused-then-recreated tmp), never on the
    /// symlink's destination.
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_tmp_path_is_not_followed() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("cowboy-fs-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.yaml");
        let victim = dir.join("VICTIM");
        std::fs::write(&victim, "original").unwrap();

        // Plant a symlink where write_atomic will place its tmp file.
        let tmp = path.with_extension("yaml.tmp");
        symlink(&victim, &tmp).unwrap();

        write_atomic(&path, b"payload").unwrap();

        // The victim was NOT overwritten through the symlink.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "original",
            "the planted symlink was followed — TOCTOU not closed"
        );
        // The real target got the content.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "payload");
        std::fs::remove_dir_all(&dir).ok();
    }
}
