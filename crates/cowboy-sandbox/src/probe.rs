//! The host-facts seam.
//!
//! Everything the plan needs to know about the real filesystem, behind a trait so
//! the plan builder stays pure. Tests supply a [`FakeHost`] describing a
//! filesystem rather than creating one, which is what lets the security-relevant
//! cases (a mask applied, a credential path refused) be ordinary unit tests.

use std::path::{Path, PathBuf};

/// Host facts the plan builder consults.
pub trait HostProbe {
    /// Whether a path exists. Optional grants and credential mounts are skipped
    /// when absent, so this decides what makes it into the plan.
    fn exists(&self, path: &Path) -> bool;

    /// The shared git directory when `root` is a **linked worktree**, else `None`.
    ///
    /// A linked worktree's `.git` is a *file* pointing into the main repository's
    /// git dir, which lives outside the project — so without binding it at its own
    /// host path, in-sandbox git cannot resolve the gitdir and every git command
    /// fails.
    fn git_common_dir(&self, root: &Path) -> Option<PathBuf>;

    /// Expand a configured path (`~`, `$VAR`) to an absolute one.
    fn expand(&self, raw: &str) -> Option<PathBuf>;

    /// The user's home directory, for denylist entries expressed relative to it.
    fn home(&self) -> Option<PathBuf>;

    /// The running `cowboy` binary, which the agent must not be able to overwrite.
    fn self_exe(&self) -> Option<PathBuf>;
}

/// A [`HostProbe`] describing a filesystem instead of touching one.
#[derive(Debug, Default, Clone)]
pub struct FakeHost {
    pub existing: Vec<PathBuf>,
    pub git_common: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub self_exe: Option<PathBuf>,
}

impl FakeHost {
    /// A fake host with a conventional home and no unusual layout.
    pub fn new() -> Self {
        Self {
            existing: Vec::new(),
            git_common: None,
            home: Some(PathBuf::from("/home/dev")),
            self_exe: Some(PathBuf::from("/usr/bin/cowboy")),
        }
    }

    pub fn with_existing<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.existing.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Present `root` as a linked worktree whose shared git dir is `common`.
    pub fn as_linked_worktree(mut self, common: impl Into<PathBuf>) -> Self {
        self.git_common = Some(common.into());
        self
    }
}

impl HostProbe for FakeHost {
    fn exists(&self, path: &Path) -> bool {
        self.existing.iter().any(|p| p == path)
    }

    fn git_common_dir(&self, _root: &Path) -> Option<PathBuf> {
        self.git_common.clone()
    }

    fn expand(&self, raw: &str) -> Option<PathBuf> {
        match raw.strip_prefix("~/") {
            Some(rest) => self.home.as_ref().map(|h| h.join(rest)),
            None if raw == "~" => self.home.clone(),
            None => Some(PathBuf::from(raw)),
        }
    }

    fn home(&self) -> Option<PathBuf> {
        self.home.clone()
    }

    fn self_exe(&self) -> Option<PathBuf> {
        self.self_exe.clone()
    }
}
