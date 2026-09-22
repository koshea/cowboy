//! Safe filesystem operations for stores inside the agent-writable workspace.
//!
//! Ranch operations use [`Dir`] so every repository-controlled component is
//! resolved relative to an already-open directory with `O_NOFOLLOW`. Keeping the
//! directory descriptor through read/write/rename also closes path-replacement
//! races that a canonicalize-then-use design would leave open.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

static UNIQUE: AtomicU64 = AtomicU64::new(0);

fn invalid(context: impl std::fmt::Display, error: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{context}: {error}"))
}

fn component(name: &OsStr) -> Result<CString> {
    let path = Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(Error::Invalid(format!(
            "unsafe filesystem component {:?}",
            name
        )));
    }
    CString::new(name.as_bytes()).map_err(|e| invalid("filesystem component contains NUL", e))
}

fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|e| invalid("path contains NUL", e))
}

fn file_from_fd(fd: libc::c_int, context: &str) -> Result<File> {
    if fd < 0 {
        Err(invalid(context, std::io::Error::last_os_error()))
    } else {
        // SAFETY: a successful open/openat returns a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn require_kind(file: &File, kind: libc::mode_t, context: &str) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` points to writable storage and `file` owns a valid fd.
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(invalid(context, std::io::Error::last_os_error()));
    }
    // SAFETY: fstat initialized `stat` on success.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != kind {
        return Err(Error::Invalid(format!("{context}: unexpected file type")));
    }
    Ok(())
}

/// An open directory used as the anchor for no-follow, descriptor-relative I/O.
#[derive(Debug)]
pub struct Dir {
    file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    Regular,
    Other,
}

impl Dir {
    /// Open `path` as a directory without following its final component.
    pub fn open(path: &Path) -> Result<Self> {
        let path = path_cstring(path)?;
        // SAFETY: `path` is NUL-terminated and flags require no variadic mode.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        let file = file_from_fd(fd, "opening directory")?;
        require_kind(&file, libc::S_IFDIR, "opening directory")?;
        Ok(Self { file })
    }

    pub fn open_dir(&self, name: impl AsRef<OsStr>) -> Result<Self> {
        let name = component(name.as_ref())?;
        // SAFETY: the parent fd and component are valid; no variadic mode is needed.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        let file = file_from_fd(fd, "opening child directory")?;
        require_kind(&file, libc::S_IFDIR, "opening child directory")?;
        Ok(Self { file })
    }

    pub fn ensure_dir(&self, name: impl AsRef<OsStr>) -> Result<Self> {
        let raw = name.as_ref();
        let name = component(raw)?;
        // SAFETY: the parent fd and component are valid.
        let rc = unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o755) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(invalid(format!("creating directory {raw:?}"), error));
            }
        }
        self.open_dir(raw)
    }

    /// Open a relative directory path one component at a time, refusing symlinks.
    pub fn open_dir_path(&self, path: &Path) -> Result<Self> {
        let mut current = self.try_clone()?;
        for part in path.components() {
            let Component::Normal(name) = part else {
                return Err(Error::Invalid(format!("unsafe relative path {path:?}")));
            };
            current = current.open_dir(name)?;
        }
        Ok(current)
    }

    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            file: self
                .file
                .try_clone()
                .map_err(|e| invalid("cloning directory fd", e))?,
        })
    }

    pub fn open_regular(&self, name: impl AsRef<OsStr>) -> Result<File> {
        self.open_regular_impl(name.as_ref(), false)?
            .ok_or_else(|| Error::Invalid("file disappeared while opening".into()))
    }

    pub fn open_regular_optional(&self, name: impl AsRef<OsStr>) -> Result<Option<File>> {
        self.open_regular_impl(name.as_ref(), true)
    }

    fn open_regular_impl(&self, name: &OsStr, optional: bool) -> Result<Option<File>> {
        let name = component(name)?;
        // SAFETY: the parent fd and component are valid; no variadic mode is needed.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if optional && error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(invalid("opening regular file", error));
        }
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        require_kind(&file, libc::S_IFREG, "opening regular file")?;
        Ok(Some(file))
    }

    /// Open a relative regular-file path without following any component.
    pub fn open_regular_path(&self, path: &Path) -> Result<File> {
        let Some(name) = path.file_name() else {
            return Err(Error::Invalid(format!("unsafe relative path {path:?}")));
        };
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let dir = if parent.as_os_str().is_empty() {
            self.try_clone()?
        } else {
            self.open_dir_path(parent)?
        };
        dir.open_regular(name)
    }

    pub fn read_to_string(&self, name: impl AsRef<OsStr>) -> Result<String> {
        let mut file = self.open_regular(name)?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|e| invalid("reading regular file", e))?;
        Ok(text)
    }

    pub fn read_to_string_optional(&self, name: impl AsRef<OsStr>) -> Result<Option<String>> {
        let Some(mut file) = self.open_regular_optional(name)? else {
            return Ok(None);
        };
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|e| invalid("reading regular file", e))?;
        Ok(Some(text))
    }

    pub fn create_regular(&self, name: impl AsRef<OsStr>, mode: libc::mode_t) -> Result<File> {
        let name = component(name.as_ref())?;
        // SAFETY: the parent fd and component are valid; O_CREAT supplies `mode`.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode,
            )
        };
        let file = file_from_fd(fd, "creating regular file")?;
        require_kind(&file, libc::S_IFREG, "creating regular file")?;
        Ok(file)
    }

    /// Open or create a persistent regular file without following the final name.
    pub fn open_regular_create(&self, name: impl AsRef<OsStr>, mode: libc::mode_t) -> Result<File> {
        let name = component(name.as_ref())?;
        // SAFETY: the parent fd and component are valid; O_CREAT supplies `mode`.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW,
                mode,
            )
        };
        let file = file_from_fd(fd, "opening regular file")?;
        require_kind(&file, libc::S_IFREG, "opening regular file")?;
        Ok(file)
    }

    /// Atomically replace `name` using a unique sibling and `renameat`.
    pub fn write_atomic(&self, name: impl AsRef<OsStr>, contents: &[u8]) -> Result<()> {
        let name = name.as_ref();
        component(name)?;
        let stem = name.to_string_lossy();
        let tmp_name = format!(
            ".{stem}.tmp.{}.{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        );
        let mut tmp = self.create_regular(&tmp_name, 0o644)?;
        let result = (|| {
            tmp.write_all(contents)
                .map_err(|e| invalid("writing temporary file", e))?;
            tmp.sync_all()
                .map_err(|e| invalid("syncing temporary file", e))?;
            drop(tmp);
            self.rename(&tmp_name, self, name)?;
            self.sync()
        })();
        if result.is_err() {
            let _ = self.unlink(&tmp_name, false);
        }
        result
    }

    /// Create a uniquely named child staging directory.
    pub fn create_unique_dir(&self, prefix: &str) -> Result<(OsString, Self)> {
        component(OsStr::new(prefix))?;
        loop {
            let name = OsString::from(format!(
                "{prefix}.{}.{}",
                std::process::id(),
                UNIQUE.fetch_add(1, Ordering::Relaxed)
            ));
            let c_name = component(&name)?;
            // SAFETY: the parent fd and component are valid.
            let rc = unsafe { libc::mkdirat(self.file.as_raw_fd(), c_name.as_ptr(), 0o755) };
            if rc == 0 {
                return Ok((name.clone(), self.open_dir(&name)?));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(invalid("creating staging directory", error));
            }
        }
    }

    pub fn entry_kind(&self, name: impl AsRef<OsStr>) -> Result<Option<EntryKind>> {
        let name = component(name.as_ref())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: arguments are valid and AT_SYMLINK_NOFOLLOW inspects the name itself.
        let rc = unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(invalid("inspecting directory entry", error));
        }
        // SAFETY: fstatat initialized `stat` on success.
        let mode = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
        Ok(Some(match mode {
            libc::S_IFDIR => EntryKind::Directory,
            libc::S_IFREG => EntryKind::Regular,
            _ => EntryKind::Other,
        }))
    }

    pub fn rename(
        &self,
        old_name: impl AsRef<OsStr>,
        new_dir: &Dir,
        new_name: impl AsRef<OsStr>,
    ) -> Result<()> {
        let old_name = component(old_name.as_ref())?;
        let new_name = component(new_name.as_ref())?;
        // SAFETY: both directory descriptors and component names are valid.
        let rc = unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                old_name.as_ptr(),
                new_dir.file.as_raw_fd(),
                new_name.as_ptr(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(invalid(
                "renaming directory entry",
                std::io::Error::last_os_error(),
            ))
        }
    }

    /// Atomically move a name only if the destination does not exist.
    pub fn rename_noreplace(
        &self,
        old_name: impl AsRef<OsStr>,
        new_name: impl AsRef<OsStr>,
    ) -> Result<()> {
        let old_name = component(old_name.as_ref())?;
        let new_name = component(new_name.as_ref())?;
        // SAFETY: Linux renameat2 consumes the supplied directory fd and names.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                self.file.as_raw_fd(),
                old_name.as_ptr(),
                self.file.as_raw_fd(),
                new_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(invalid(
                "publishing directory entry",
                std::io::Error::last_os_error(),
            ))
        }
    }

    /// Atomically exchange two names in one directory without following either.
    pub fn exchange(&self, first: impl AsRef<OsStr>, second: impl AsRef<OsStr>) -> Result<()> {
        let first = component(first.as_ref())?;
        let second = component(second.as_ref())?;
        // SAFETY: Linux renameat2 consumes the supplied directory fds and names.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                self.file.as_raw_fd(),
                first.as_ptr(),
                self.file.as_raw_fd(),
                second.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(invalid(
                "exchanging directory entries",
                std::io::Error::last_os_error(),
            ))
        }
    }

    pub fn sync(&self) -> Result<()> {
        self.file
            .sync_all()
            .map_err(|e| invalid("syncing directory", e))
    }

    pub fn list_names(&self) -> Result<Vec<OsString>> {
        // SAFETY: dup returns a new descriptor consumed by fdopendir/closedir.
        let duplicate = unsafe { libc::dup(self.file.as_raw_fd()) };
        if duplicate < 0 {
            return Err(invalid(
                "duplicating directory fd",
                std::io::Error::last_os_error(),
            ));
        }
        // SAFETY: duplicate is an owned directory descriptor.
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            // SAFETY: fdopendir did not consume the descriptor on failure.
            unsafe { libc::close(duplicate) };
            return Err(invalid(
                "opening directory stream",
                std::io::Error::last_os_error(),
            ));
        }
        let mut names = Vec::new();
        loop {
            // SAFETY: stream remains valid until closed below.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            // SAFETY: d_name is NUL-terminated for a successful readdir entry.
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes != b"." && bytes != b".." {
                names.push(OsString::from_vec(bytes.to_vec()));
            }
        }
        // SAFETY: stream was returned by fdopendir and has not been closed.
        if unsafe { libc::closedir(stream) } != 0 {
            return Err(invalid(
                "closing directory stream",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(names)
    }

    /// Remove one entry recursively, never following symlinks encountered within it.
    pub fn remove_tree(&self, name: impl AsRef<OsStr>) -> Result<()> {
        let name = name.as_ref();
        match self.entry_kind(name)? {
            None => Ok(()),
            Some(EntryKind::Directory) => {
                let child = self.open_dir(name)?;
                for entry in child.list_names()? {
                    child.remove_tree(entry)?;
                }
                self.unlink(name, true)
            }
            Some(_) => self.unlink(name, false),
        }
    }

    fn unlink(&self, name: impl AsRef<OsStr>, directory: bool) -> Result<()> {
        let name = component(name.as_ref())?;
        // SAFETY: the parent descriptor and component are valid.
        let rc = unsafe {
            libc::unlinkat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                if directory { libc::AT_REMOVEDIR } else { 0 },
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(invalid(
                "removing directory entry",
                std::io::Error::last_os_error(),
            ))
        }
    }
}

/// Atomically write `contents` to `path` without following the final temporary file.
///
/// Ranch code uses [`Dir::write_atomic`] directly so all repository-controlled
/// ancestors are pinned too. This path wrapper remains for stores whose parent is
/// not repository-controlled in the same way.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| invalid("creating parent directory", e))?;
    let dir = Dir::open(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::Invalid(format!("path has no filename: {}", path.display())))?;
    dir.write_atomic(name, contents)
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
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_components_and_regular_files_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "cowboy-fs-symlink-{}-{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        ));
        let victim = root.join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("data"), "secret").unwrap();
        let anchor = root.join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        symlink(&victim, anchor.join("linked")).unwrap();
        symlink(victim.join("data"), anchor.join("file")).unwrap();

        let dir = Dir::open(&anchor).unwrap();
        assert!(dir.open_dir("linked").is_err());
        assert!(dir.open_regular("file").is_err());
        assert_eq!(
            std::fs::read_to_string(victim.join("data")).unwrap(),
            "secret"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn regular_file_reads_work_and_fifo_reads_do_not_block() {
        use std::os::unix::ffi::OsStrExt;
        use std::time::Duration;

        let root = std::env::temp_dir().join(format!(
            "cowboy-fs-fifo-{}-{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("regular"), "contents").unwrap();
        let fifo_path = root.join("fifo");
        let fifo = CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let dir = Dir::open(&root).unwrap();
        assert_eq!(dir.read_to_string("regular").unwrap(), "contents");
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(dir.open_regular("fifo").is_err());
        });
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("opening a FIFO for inspection must not block"),
            "FIFO must still be rejected as non-regular"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
