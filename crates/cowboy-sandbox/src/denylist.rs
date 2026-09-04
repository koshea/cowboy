//! Paths the agent may never obtain at runtime, at any approval scope.
//!
//! Runtime path grants exist so a user can hand the agent a folder mid-task
//! without restarting the session. That convenience must not become a softer route
//! to credentials than the one already designed for them.
//!
//! `cowboy secrets add <preset>` is **non-destructive on purpose**: it prints a
//! grant the user then adds to host-owned config themselves. An in-flow approval
//! modal is a much weaker gate — the user is mid-task, focused on something else,
//! and habituation does the rest: after nine reasonable approvals, the tenth is
//! reflexive. So anything `cowboy secrets` covers is refused here and the user is
//! pointed back at that command.
//!
//! The preset half of the list is **derived** from
//! [`cowboy_core::presets`] rather than restated, so adding a preset extends the
//! denylist automatically instead of the two silently diverging.

use std::path::{Path, PathBuf};

use crate::probe::HostProbe;

/// Why a path was refused, so the refusal can say something useful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// Covered by a `cowboy secrets` preset — that command is the way in.
    Credential { preset_path: String },
    /// A secret store with no preset (GPG keys, browser profiles, and the like).
    SensitiveStore { what: &'static str },
    /// Cowboy's own provider credentials. The agent must never reach these; they
    /// are the one secret deliberately kept out of every project.
    ProviderCredentials,
    /// The `cowboy` binary itself — writable access would mean arbitrary host
    /// code execution on the next invocation.
    CowboyBinary,
    /// Host-owned security configuration. Reading it would tell the agent exactly
    /// what its boundary is; writing it would let the agent choose.
    HostOwnedConfig,
}

impl DenyReason {
    /// Does this reason still apply when the exposure is **read-only**?
    ///
    /// True for everything except [`DenyReason::CowboyBinary`], whose whole hazard is
    /// *write* access — "write access to it is arbitrary host code execution on the
    /// next run". The agent can already read and execute the cowboy binary by design:
    /// the plan binds it read-only at `SHIM_PATH`, and a system install puts it inside
    /// the read-only `/usr` bind too. Refusing a read-only bind of a directory that
    /// happens to contain it would protect nothing and would exclude `~/.cargo/bin` on
    /// every machine where cowboy was installed with `cargo install` — which is most
    /// of them.
    ///
    /// Every other reason is a *read* hazard: credentials, secret stores, provider
    /// keys, and the host-owned config that defines the boundary are all compromised
    /// by being read, so read-only changes nothing about them.
    pub fn blocks_read_only(&self) -> bool {
        !matches!(self, DenyReason::CowboyBinary)
    }

    /// A message that tells the user what to do instead, not just "no".
    pub fn explain(&self) -> String {
        match self {
            DenyReason::Credential { preset_path } => format!(
                "{preset_path} holds credentials. Runtime grants cannot cover these — \
                 run `cowboy secrets add <preset>` and add the printed grant to \
                 .cowboy/security.yaml yourself, so the decision is deliberate."
            ),
            DenyReason::SensitiveStore { what } => format!(
                "refusing to grant {what}: a secret store cannot be approved from \
                 inside a task. Add an explicit grant to .cowboy/security.yaml if you \
                 really intend it."
            ),
            DenyReason::ProviderCredentials => {
                "refusing to grant cowboy's provider credentials (API keys). These are \
                 kept out of every project by design and are never reachable from a sandbox."
                    .into()
            }
            DenyReason::CowboyBinary => {
                "refusing to grant the cowboy binary: write access to it is arbitrary \
                 host code execution on the next run."
                    .into()
            }
            DenyReason::HostOwnedConfig => {
                "refusing to grant host-owned security config. It defines the boundary, \
                 so the sandboxed agent must not be able to read or change it."
                    .into()
            }
        }
    }
}

/// One denied path and why.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    reason: DenyReason,
}

/// The set of paths no runtime grant may cover.
#[derive(Debug, Clone, Default)]
pub struct Denylist {
    entries: Vec<Entry>,
}

/// Secret stores with no `cowboy secrets` preset. Presets cover the tools users
/// legitimately want the agent to use; these are stores with no such story, where
/// the answer is an explicit host-owned grant or nothing.
const SENSITIVE_STORES: &[(&str, &str)] = &[
    ("~/.gnupg", "your GPG keyring"),
    ("~/.password-store", "your password store"),
    (
        "~/.mozilla",
        "a Firefox profile (saved passwords, cookies, session tokens)",
    ),
    (
        "~/.config/google-chrome",
        "a Chrome profile (saved passwords, cookies)",
    ),
    (
        "~/.config/chromium",
        "a Chromium profile (saved passwords, cookies)",
    ),
    (
        "~/.config/BraveSoftware",
        "a Brave profile (saved passwords, cookies)",
    ),
    ("~/.local/share/keyrings", "your login keyring"),
    ("~/.docker/config.json", "your Docker registry credentials"),
    ("~/.netrc", "your .netrc credentials"),
    ("~/.npmrc", "your npm tokens"),
    ("~/.pypirc", "your PyPI tokens"),
    ("~/.cargo/credentials.toml", "your crates.io token"),
];

impl Denylist {
    /// Build the denylist for a host.
    ///
    /// Absolute where possible: entries whose `~` cannot be expanded are dropped,
    /// because a relative entry could never match a canonicalized request anyway
    /// and keeping it would only create the illusion of coverage.
    ///
    /// `project_root` is needed for one exception — see the `self_exe` handling
    /// below.
    pub fn build(probe: &dyn HostProbe, project_root: &Path) -> Self {
        let mut entries = Vec::new();

        // Derived from the preset table — the whole point, so the two can't drift.
        for src in cowboy_core::presets::all_credential_sources() {
            if let Some(path) = probe.expand(src) {
                entries.push(Entry {
                    path,
                    reason: DenyReason::Credential {
                        preset_path: src.to_string(),
                    },
                });
            }
        }

        for (raw, what) in SENSITIVE_STORES {
            if let Some(path) = probe.expand(raw) {
                entries.push(Entry {
                    path,
                    reason: DenyReason::SensitiveStore { what },
                });
            }
        }

        // Provider credentials: endpoint URLs and API keys, home-only by design.
        if let Some(home) = probe.home() {
            entries.push(Entry {
                path: home.join(".config/cowboy"),
                reason: DenyReason::ProviderCredentials,
            });
        }

        // Protect the cowboy binary from a runtime grant that would make it
        // writable — that is arbitrary host code execution on the next run.
        //
        // Exception: when the binary lives *inside* the project (a development
        // checkout of cowboy itself, where it is a build artifact under `target/`),
        // the entry is skipped. The project is writable by design, so the entry
        // could not protect anything — but the ancestor rule below would refuse the
        // whole project root, making cowboy unable to work on itself.
        if let Some(exe) = probe.self_exe() {
            if !exe.starts_with(project_root) {
                entries.push(Entry {
                    path: exe,
                    reason: DenyReason::CowboyBinary,
                });
            }
        }

        Self { entries }
    }

    /// Why `path` is refused, or `None` if it is allowed.
    ///
    /// Matches a denied path **and everything under it**, and also refuses any
    /// *ancestor* of a denied path: granting `~` must not become a way to reach
    /// `~/.aws` one level down.
    ///
    /// `.` and `..` are normalized away first. They used to be the caller's problem,
    /// and the callers did not all agree: grant paths are canonicalized by both
    /// entry points (`cowboy grant` and the agent's `request_path`), but mount sources
    /// arrive from `plan::resolve_source`, which only joins. `/home/dev/.config/../.aws`
    /// therefore did not match the denied `/home/dev/.aws` and was bound into the
    /// sandbox. Normalizing here rather than asking every caller to remember is the
    /// difference between an invariant and a convention.
    ///
    /// Note what this still does **not** do: resolve symlinks. That needs the
    /// filesystem, and this runs against a fakeable `HostProbe` in tests and on paths
    /// that may not exist yet. Callers that can canonicalize should — both grant paths
    /// do — and this is the backstop for the ones that cannot.
    pub fn check(&self, path: &Path) -> Option<DenyReason> {
        let path = &normalize(path);
        // Specific entries first: "this is your AWS credentials, use cowboy
        // secrets" is more actionable than "this is host-owned config", and both
        // match for e.g. `~/.config/cowboy/providers.yaml`.
        for e in &self.entries {
            if path.starts_with(&e.path) || e.path.starts_with(path) {
                return Some(e.reason.clone());
            }
        }
        // Host-owned config is then refused by name at any depth: `.cowboy` may
        // sit inside a project the user legitimately grants, so a prefix test on a
        // fixed absolute path would miss it.
        if names_host_owned_config(path) {
            return Some(DenyReason::HostOwnedConfig);
        }
        None
    }

    /// Number of entries, for tests and `cowboy sandbox plan` output.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Denied paths, sorted, for display and snapshotting.
    pub fn paths(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = self.entries.iter().map(|e| e.path.as_path()).collect();
        v.sort_unstable();
        v
    }
}

/// Whether a path is, or is inside, cowboy's host-owned config directory, or is
/// one of the host-owned files themselves.
/// Collapse `.` and `..` lexically, without touching the filesystem.
///
/// `Path::components()` already drops `.`, so only `..` needs work: each one pops the
/// previous normal component. A `..` that would escape the root is dropped, matching
/// how the kernel treats `/..` — and meaning the result can never be shorter than the
/// root, so this cannot turn a denied absolute path into a relative one.
///
/// Purely lexical on purpose: this runs in the plan builder against a fakeable
/// `HostProbe`, and on mount sources that need not exist yet, so `canonicalize` is not
/// available. It is strictly better than nothing — the bug it fixes was a `..` walking
/// sideways out of a denied prefix — and callers that *can* canonicalize still should.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                // Only pop a real directory name; keep the root and any leading `..`
                // of a relative path, which have nothing above them to discard.
                if out
                    .components()
                    .next_back()
                    .is_some_and(|c| matches!(c, Component::Normal(_)))
                {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn names_host_owned_config(path: &Path) -> bool {
    use cowboy_core::config::{COWBOY_DIR, MODELS_FILE, PROVIDERS_FILE, SECURITY_FILE};
    path.components().any(|c| c.as_os_str() == COWBOY_DIR)
        || path
            .file_name()
            .is_some_and(|n| n == SECURITY_FILE || n == MODELS_FILE || n == PROVIDERS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::FakeHost;

    /// `..` and `.` are collapsed before any prefix test, and nothing can escape the
    /// root — otherwise normalization itself could turn a denied absolute path into
    /// something that no longer matches.
    #[test]
    fn normalize_collapses_traversal_without_escaping_the_root() {
        let n = |s: &str| normalize(Path::new(s));
        assert_eq!(
            n("/home/dev/.config/../.aws"),
            PathBuf::from("/home/dev/.aws")
        );
        assert_eq!(n("/home/dev/./.aws"), PathBuf::from("/home/dev/.aws"));
        assert_eq!(n("/home/dev/.aws/"), PathBuf::from("/home/dev/.aws"));
        assert_eq!(
            n("/home/dev/a/b/../../.aws"),
            PathBuf::from("/home/dev/.aws")
        );
        // `..` past the root is dropped, as the kernel treats `/..`.
        assert_eq!(n("/../../.aws"), PathBuf::from("/.aws"));
        assert_eq!(n("/.."), PathBuf::from("/"));
        // A relative path keeps its leading `..`: there is nothing above it to pop,
        // and silently dropping them would change which directory it names.
        assert_eq!(n("../../x"), PathBuf::from("../../x"));
        assert_eq!(n("a/../b"), PathBuf::from("b"));
        // Already-clean paths are unchanged.
        assert_eq!(n("/home/dev/.aws"), PathBuf::from("/home/dev/.aws"));
    }

    /// The traversal that motivated normalizing here: a sideways `..` out of a
    /// non-denied sibling into a denied directory. `/home/dev/.aws/../.aws` was always
    /// caught (it literally starts with the denied prefix); this form was not.
    #[test]
    fn a_sideways_traversal_into_a_denied_path_is_refused() {
        let l = list();
        assert!(l.check(Path::new("/home/dev/.config/../.aws")).is_some());
        assert!(l
            .check(Path::new("/home/dev/x/y/../../.aws/credentials"))
            .is_some());
        // And a path that merely mentions `..` without reaching anything denied is
        // still allowed — normalization must not become its own denial.
        assert!(l.check(Path::new("/srv/proj/sub/../src")).is_none());
    }

    /// A denylist for a project unrelated to the user's home, which is the
    /// ordinary case.
    fn list() -> Denylist {
        Denylist::build(&FakeHost::new(), Path::new("/srv/work/proj"))
    }

    /// The requirement that motivated the denylist: every credential path
    /// `cowboy secrets` knows about is refused, at every scope.
    #[test]
    fn every_preset_credential_path_is_refused() {
        let d = list();
        for src in cowboy_core::presets::all_credential_sources() {
            let abs = FakeHost::new().expand(src).unwrap();
            let reason = d.check(&abs);
            assert!(
                matches!(reason, Some(DenyReason::Credential { .. })),
                "{src} ({}) was not refused as a credential: {reason:?}",
                abs.display()
            );
        }
    }

    #[test]
    fn refuses_paths_under_a_denied_directory() {
        let d = list();
        assert!(d.check(Path::new("/home/dev/.aws/credentials")).is_some());
        assert!(d.check(Path::new("/home/dev/.ssh/id_ed25519")).is_some());
        assert!(d
            .check(Path::new("/home/dev/.config/gh/hosts.yml"))
            .is_some());
    }

    /// Granting a parent must not be a way to reach a denied child.
    #[test]
    fn refuses_ancestors_of_denied_paths() {
        let d = list();
        assert!(
            d.check(Path::new("/home/dev")).is_some(),
            "granting the whole home dir would expose every credential under it"
        );
    }

    #[test]
    fn refuses_provider_credentials_and_the_cowboy_binary() {
        let d = list();
        assert_eq!(
            d.check(Path::new("/home/dev/.config/cowboy/providers.yaml")),
            Some(DenyReason::ProviderCredentials)
        );
        assert_eq!(
            d.check(Path::new("/usr/bin/cowboy")),
            Some(DenyReason::CowboyBinary)
        );
    }

    /// Granting a directory that *contains* the cowboy binary must still be
    /// refused — `cowboy grant /usr/bin --rw` would otherwise be a way to make it
    /// writable.
    #[test]
    fn refuses_a_directory_containing_the_cowboy_binary() {
        let d = list();
        assert_eq!(
            d.check(Path::new("/usr/bin")),
            Some(DenyReason::CowboyBinary)
        );
    }

    /// Cowboy must be able to work on its own checkout. There the binary is a
    /// build artifact under `target/`, so the binary entry would make the ancestor
    /// rule refuse the entire project — and it could not protect anything anyway,
    /// since the project is writable by design.
    #[test]
    fn a_dev_checkout_of_cowboy_is_not_refused_by_its_own_binary() {
        let root = Path::new("/home/dev/src/cowboy");
        let probe = FakeHost {
            self_exe: Some(root.join("target/debug/cowboy")),
            ..FakeHost::new()
        };
        let d = Denylist::build(&probe, root);
        assert_eq!(d.check(root), None, "cowboy must be able to build itself");
        assert_eq!(d.check(&root.join("crates/cowboy-cli")), None);
        // An installed binary elsewhere is still protected.
        let other = FakeHost::new();
        let d2 = Denylist::build(&other, Path::new("/srv/proj"));
        assert_eq!(
            d2.check(Path::new("/usr/bin/cowboy")),
            Some(DenyReason::CowboyBinary)
        );
    }

    #[test]
    fn refuses_sensitive_stores_without_a_preset() {
        let d = list();
        for p in [
            "/home/dev/.gnupg",
            "/home/dev/.mozilla/firefox",
            "/home/dev/.netrc",
            "/home/dev/.npmrc",
        ] {
            assert!(d.check(Path::new(p)).is_some(), "{p} should be refused");
        }
    }

    /// Host-owned config is refused wherever it appears, including inside a
    /// project the user has legitimately granted.
    #[test]
    fn refuses_host_owned_config_at_any_depth() {
        let d = list();
        for p in [
            "/srv/work/proj/.cowboy",
            "/srv/work/proj/.cowboy/security.yaml",
            "/srv/work/proj/nested/.cowboy/models.yaml",
            "/somewhere/security.yaml",
            "/somewhere/providers.yaml",
        ] {
            assert_eq!(
                d.check(Path::new(p)),
                Some(DenyReason::HostOwnedConfig),
                "{p} should be refused as host-owned config"
            );
        }
    }

    #[test]
    fn allows_ordinary_project_paths() {
        let d = list();
        for p in [
            "/srv/work/other-repo",
            "/srv/work/other-repo/src/main.rs",
            "/home/dev/src/scratch",
            "/tmp/build",
        ] {
            assert_eq!(d.check(Path::new(p)), None, "{p} should be allowed");
        }
    }

    /// Refusals must say what to do instead; a bare "denied" trains users to
    /// look for a way around it.
    #[test]
    fn refusals_point_at_cowboy_secrets() {
        let d = list();
        let reason = d.check(Path::new("/home/dev/.aws")).unwrap();
        assert!(reason.explain().contains("cowboy secrets add"));
    }

    #[test]
    fn entries_are_dropped_when_home_cannot_be_resolved() {
        let probe = FakeHost {
            home: None,
            self_exe: None,
            ..FakeHost::default()
        };
        let d = Denylist::build(&probe, Path::new("/srv/work/proj"));
        // No `~`-relative entry survives, but host-owned config is still refused
        // because that check is by name, not by absolute path.
        assert!(d.is_empty());
        assert_eq!(
            d.check(Path::new("/p/.cowboy/security.yaml")),
            Some(DenyReason::HostOwnedConfig)
        );
    }
}
