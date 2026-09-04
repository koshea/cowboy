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
    /// Resolve a configured path, expanding a leading `~`.
    ///
    /// Must agree with [`Self::home`]. The denylist resolves credential sources
    /// (`~/.aws`, `~/.ssh`, …) through this method and separately uses `home()` to
    /// cover the home directory itself, so an implementation where the two disagree
    /// silently shrinks the denylist rather than failing.
    fn expand(&self, raw: &str) -> Option<PathBuf>;

    /// The user's home directory, for denylist entries expressed relative to it.
    fn home(&self) -> Option<PathBuf>;

    /// The running `cowboy` binary, which the agent must not be able to overwrite.
    fn self_exe(&self) -> Option<PathBuf>;

    /// Resolve a path to its canonical form, following symlinks.
    ///
    /// The denylist's own normalization is purely lexical (it runs against paths that
    /// may not exist and a fakeable probe), so it cannot catch a *symlink* that points
    /// a benign-looking source at `~/.aws` or `~/.config/cowboy`. A caller that binds a
    /// real path — a mount source, a credential grant — must resolve it here first, so
    /// the denylist sees the true destination. Returns `None` when the path cannot be
    /// resolved (it does not exist, or a component is not traversable); a caller binding
    /// a *required* source must treat that as an error rather than trusting the literal.
    fn canonicalize(&self, path: &Path) -> Option<PathBuf>;
}

/// A [`HostProbe`] describing a filesystem instead of touching one.
#[derive(Debug, Default, Clone)]
pub struct FakeHost {
    pub existing: Vec<PathBuf>,
    pub git_common: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub self_exe: Option<PathBuf>,
    /// Symlink redirections: a source path here canonicalizes to its target, so a
    /// test can point a benign-looking mount source at a denied destination without
    /// touching a real filesystem.
    pub symlinks: Vec<(PathBuf, PathBuf)>,
}

impl FakeHost {
    /// A fake host with a conventional home and no unusual layout.
    pub fn new() -> Self {
        Self {
            existing: Vec::new(),
            git_common: None,
            home: Some(PathBuf::from("/home/dev")),
            self_exe: Some(PathBuf::from("/usr/bin/cowboy")),
            symlinks: Vec::new(),
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

    /// Point the home directory at `home`, so a test can place a real credential
    /// store on disk and have the denylist recognise it — without depending on
    /// whatever happens to exist in the developer's own home.
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Present `root` as a linked worktree whose shared git dir is `common`.
    pub fn as_linked_worktree(mut self, common: impl Into<PathBuf>) -> Self {
        self.git_common = Some(common.into());
        self
    }

    /// Add a symlink: `source` canonicalizes to `target`. Also marks `source` as
    /// existing, so a test need only state the redirection.
    pub fn with_symlink(mut self, source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        let source = source.into();
        let target = target.into();
        self.existing.push(source.clone());
        self.symlinks.push((source, target));
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

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        // A symlink on the path (or an ancestor of it) redirects the whole subtree:
        // `/home/dev/link` -> `/secret` makes `/home/dev/link/x` resolve to `/secret/x`.
        if let Some(redirected) = self
            .symlinks
            .iter()
            .find_map(|(src, dst)| path.strip_prefix(src).ok().map(|rest| dst.join(rest)))
        {
            return Some(redirected);
        }
        // No symlink: a real `canonicalize` only succeeds for paths that exist.
        if self.exists(path) {
            Some(path.to_path_buf())
        } else {
            None
        }
    }
}
