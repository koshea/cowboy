//! Ranch Plans: a large task decomposed into coordinated, dependency-aware
//! workstreams, each run as a Cowboy session in its own worktree/branch.
//!
//! A ranch's plan is the **committed source of truth** at
//! `.cowboy/ranches/<id>/ranch.yaml` (the agent never edits it — only the user
//! or, with approval, the coordinator). Runtime event/scratch files alongside it
//! are gitignored. This module owns the on-disk schema + readiness logic; the
//! daemon/CLI layer drives launching and coordination.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

fn default_version() -> u32 {
    1
}

/// Lifecycle of a whole ranch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RanchStatus {
    Planning,
    Ready,
    Running,
    WaitingForUser,
    Paused,
    Integrating,
    Complete,
    Failed,
    Cancelled,
}

/// Lifecycle of one workstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkstreamStatus {
    /// Defined but not yet evaluated for readiness.
    Planned,
    /// Dependencies not yet complete.
    Blocked,
    /// Dependencies satisfied; can be started.
    Ready,
    Starting,
    Running,
    WaitingForUser,
    Complete,
    Failed,
    Cancelled,
    MergeReady,
    Integrated,
}

impl WorkstreamStatus {
    /// A workstream whose outputs downstream deps can rely on.
    pub fn is_done(self) -> bool {
        matches!(
            self,
            WorkstreamStatus::Complete
                | WorkstreamStatus::MergeReady
                | WorkstreamStatus::Integrated
        )
    }
}

/// One workstream within a ranch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workstream {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub goal: String,
    /// Workstream ids this one depends on (must be done before it can start).
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "default_planned")]
    pub status: WorkstreamStatus,
    /// The session running this workstream (set once started).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    /// Artifacts this workstream is expected to publish (names, not paths).
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
    /// Acceptance criteria (human-readable).
    #[serde(default)]
    pub acceptance: Vec<String>,
}

fn default_planned() -> WorkstreamStatus {
    WorkstreamStatus::Planned
}

impl Workstream {
    /// Are all of this workstream's dependencies in `done`?
    pub fn deps_satisfied(&self, done: &HashSet<String>) -> bool {
        self.depends_on.iter().all(|d| done.contains(d))
    }
}

/// A ranch plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ranch {
    #[serde(default = "default_version")]
    pub version: u32,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default = "default_planning")]
    pub status: RanchStatus,
    #[serde(default)]
    pub workstreams: Vec<Workstream>,
    /// When true (the default), the daemon coordinator auto-advances the plan as
    /// workstreams finish: it reconciles, promotes outputs, and launches newly
    /// ready workstreams without the user re-running `ranch start`. Set false to
    /// drive the plan manually. Acceptance gates still pause for sign-off.
    #[serde(default = "default_true")]
    pub auto_advance: bool,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub updated_ms: u64,
}

fn default_planning() -> RanchStatus {
    RanchStatus::Planning
}

fn default_true() -> bool {
    true
}

impl Ranch {
    /// A hash of everything that counts as the plan's **scope**, as opposed to its
    /// progress.
    ///
    /// In: the ranch's identity and goal, and for each workstream its id, title, goal,
    /// dependencies, expected artifacts and acceptance criteria. Out: status,
    /// `session_id`, `branch`, `worktree_path`, `auto_advance`, timestamps — the things
    /// the coordinator maintains as work happens.
    ///
    /// Order-sensitive on purpose: reordering the workstreams is a change to the plan
    /// as a person reads it, so it should not slip through a progress write.
    ///
    /// Used by [`save_progress`] to make the "scope changes are user-gated" rule an
    /// actual check rather than a convention.
    pub fn scope_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut h);
        self.title.hash(&mut h);
        self.goal.hash(&mut h);
        for w in &self.workstreams {
            w.id.hash(&mut h);
            w.title.hash(&mut h);
            w.goal.hash(&mut h);
            w.depends_on.hash(&mut h);
            w.expected_artifacts.hash(&mut h);
            w.acceptance.hash(&mut h);
        }
        h.finish()
    }

    /// Ids of workstreams whose outputs are done.
    pub fn done_ids(&self) -> HashSet<String> {
        self.workstreams
            .iter()
            .filter(|w| w.status.is_done())
            .map(|w| w.id.clone())
            .collect()
    }

    /// Workstreams that are not yet done/started and whose deps are all done —
    /// i.e. ready to launch right now.
    pub fn ready_workstreams(&self) -> Vec<&Workstream> {
        let done = self.done_ids();
        self.workstreams
            .iter()
            .filter(|w| {
                matches!(
                    w.status,
                    WorkstreamStatus::Planned | WorkstreamStatus::Blocked | WorkstreamStatus::Ready
                ) && w.deps_satisfied(&done)
            })
            .collect()
    }

    /// Recompute Planned/Blocked/Ready from the dependency graph (does not touch
    /// running/done workstreams). Returns ids that newly became ready.
    pub fn recompute_readiness(&mut self) -> Vec<String> {
        let done = self.done_ids();
        let mut newly_ready = Vec::new();
        for w in &mut self.workstreams {
            if matches!(
                w.status,
                WorkstreamStatus::Planned | WorkstreamStatus::Blocked | WorkstreamStatus::Ready
            ) {
                let satisfied = w.depends_on.iter().all(|d| done.contains(d));
                let next = if satisfied {
                    WorkstreamStatus::Ready
                } else {
                    WorkstreamStatus::Blocked
                };
                if next == WorkstreamStatus::Ready && w.status != WorkstreamStatus::Ready {
                    newly_ready.push(w.id.clone());
                }
                w.status = next;
            }
        }
        newly_ready
    }

    pub fn workstream(&self, id: &str) -> Option<&Workstream> {
        self.workstreams.iter().find(|w| w.id == id)
    }
    pub fn workstream_mut(&mut self, id: &str) -> Option<&mut Workstream> {
        self.workstreams.iter_mut().find(|w| w.id == id)
    }

    /// Validate the dependency graph: every `depends_on` must reference a real
    /// workstream, ids must be unique, and there must be no cycle. Without this,
    /// a typo'd dep or a cycle (`a→b, b→a`) silently blocks workstreams forever
    /// (`deps_satisfied` is never true) with no error — a confusing deadlock.
    /// Call before starting a ranch.
    pub fn validate(&self) -> std::result::Result<(), String> {
        // Ids become path components host-side (`ranch_path`, `ranch_artifact_dir`)
        // and reach `remove_dir_all`/copy in the coordinator, so a traversing id is
        // a host-filesystem escape, not just a bad label. Reject before anything is
        // written.
        if !is_safe_id(&self.id) {
            return Err(format!(
                "unsafe ranch id {:?}: must be a single path component (no `/`, `..`, or empty)",
                self.id
            ));
        }
        for w in &self.workstreams {
            if !is_safe_id(&w.id) {
                return Err(format!(
                    "unsafe workstream id {:?}: must be a single path component \
                     (no `/`, `..`, or empty)",
                    w.id
                ));
            }
        }
        let ids: HashSet<&str> = self.workstreams.iter().map(|w| w.id.as_str()).collect();
        if ids.len() != self.workstreams.len() {
            return Err("duplicate workstream ids".into());
        }
        for w in &self.workstreams {
            for d in &w.depends_on {
                if !ids.contains(d.as_str()) {
                    return Err(format!(
                        "workstream {:?} depends on unknown workstream {:?}",
                        w.id, d
                    ));
                }
            }
        }
        // Cycle detection via DFS over the dependency edges.
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Visiting,
            Done,
        }
        fn visit<'a>(
            id: &'a str,
            ranch: &'a Ranch,
            state: &mut std::collections::HashMap<&'a str, Mark>,
        ) -> std::result::Result<(), String> {
            match state.get(id) {
                Some(Mark::Done) => return Ok(()),
                Some(Mark::Visiting) => return Err(format!("dependency cycle through {id:?}")),
                None => {}
            }
            state.insert(id, Mark::Visiting);
            if let Some(w) = ranch.workstream(id) {
                for d in &w.depends_on {
                    visit(d, ranch, state)?;
                }
            }
            state.insert(id, Mark::Done);
            Ok(())
        }
        let mut state = std::collections::HashMap::new();
        for w in &self.workstreams {
            visit(&w.id, self, &mut state)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Storage  (.cowboy/ranches/<id>/ranch.yaml — committed source of truth)
// ---------------------------------------------------------------------------

/// Whether `id` is safe to use as a single path component in the ranch store.
///
/// Ranch and workstream ids come from the **committed, agent-writable**
/// `ranch.yaml`, yet they are `join`ed into host-side paths that reach
/// `remove_dir_all` and file copies in the auto-advancing coordinator. A raw
/// `Path::join` treats `..` as a real parent-dir hop and an absolute component as
/// a full replacement, so an id like `../../..` or `/etc` would escape the store
/// and let a hostile repo delete or clobber arbitrary host directories. An id must
/// therefore be exactly one normal filename component — no separators, no `.`/`..`,
/// not absolute. (Same guard as [`crate::memory`]'s `is_safe_name`.)
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty() && Path::new(id).file_name() == Some(std::ffi::OsStr::new(id))
}

/// The ranches directory for a project root.
pub fn ranches_dir(root: &Path) -> PathBuf {
    root.join(".cowboy").join("ranches")
}

/// The plan file for a ranch.
pub fn ranch_path(root: &Path, id: &str) -> PathBuf {
    ranches_dir(root).join(id).join("ranch.yaml")
}

/// The committed artifact store for a workstream's promoted outputs
/// (`.cowboy/ranches/<id>/artifacts/<workstream>/`).
pub fn ranch_artifact_dir(root: &Path, ranch_id: &str, workstream_id: &str) -> PathBuf {
    ranches_dir(root)
        .join(ranch_id)
        .join("artifacts")
        .join(workstream_id)
}

/// A ranch directory pinned beneath the repository root.
///
/// The repository controls `.cowboy` and everything below it. Each component is
/// therefore opened relative to the previous descriptor with `O_NOFOLLOW`; plan
/// reads and atomic renames stay relative to the pinned ranch directory.
struct RanchDir {
    id: String,
    dir: crate::fs::Dir,
}

impl RanchDir {
    fn open(root: &Path, id: &str, create: bool) -> Result<Self> {
        if !is_safe_id(id) {
            return Err(Error::Invalid(format!(
                "unsafe ranch id {id:?}: must be a single path component"
            )));
        }
        let root = crate::fs::Dir::open(root)?;
        let cowboy = if create {
            root.ensure_dir(".cowboy")?
        } else {
            root.open_dir(".cowboy")?
        };
        let ranches = if create {
            cowboy.ensure_dir("ranches")?
        } else {
            cowboy.open_dir("ranches")?
        };
        let dir = if create {
            ranches.ensure_dir(id)?
        } else {
            ranches.open_dir(id)?
        };
        Ok(Self {
            id: id.to_string(),
            dir,
        })
    }

    fn load(&self) -> Result<Ranch> {
        let text = self
            .dir
            .read_to_string("ranch.yaml")
            .map_err(|error| Error::Invalid(format!("loading ranch `{}`: {error}", self.id)))?;
        let ranch: Ranch = serde_yaml_ng::from_str(&text)
            .map_err(|e| Error::Invalid(format!("parsing {}: {e}", self.id)))?;
        if ranch.id != self.id {
            return Err(Error::Invalid(format!(
                "ranch id mismatch: directory `{}` holds a plan with id {:?}",
                self.id, ranch.id
            )));
        }
        for w in &ranch.workstreams {
            if !is_safe_id(&w.id) {
                return Err(Error::Invalid(format!(
                    "unsafe workstream id {:?} in ranch `{}`: must be a single path component",
                    w.id, self.id
                )));
            }
        }
        Ok(ranch)
    }

    fn write_yaml(&self, yaml: &[u8]) -> Result<()> {
        self.dir.write_atomic("ranch.yaml", yaml)
    }

    fn save(&self, ranch: &Ranch) -> Result<()> {
        if ranch.id != self.id {
            return Err(Error::Invalid(format!(
                "refusing to save ranch {:?} through directory `{}`",
                ranch.id, self.id
            )));
        }
        let yaml = serde_yaml_ng::to_string(ranch).map_err(|e| Error::Invalid(e.to_string()))?;
        self.write_yaml(yaml.as_bytes())
    }

    fn save_progress(&self, before: &Ranch, after: &Ranch) -> Result<()> {
        if before.scope_fingerprint() != after.scope_fingerprint() {
            return Err(Error::Invalid(format!(
                "refusing to write ranch `{}`: this is a progress update, but the plan's scope \
                 changed. Scope changes go through a proposal and `cowboy ranch approve`",
                after.id
            )));
        }
        if let Ok(on_disk) = self.load() {
            if on_disk.scope_fingerprint() != after.scope_fingerprint() {
                return Err(Error::Invalid(format!(
                    "refusing to write ranch `{}`: its scope changed on disk since it was loaded \
                     (a proposal was approved concurrently). Re-run so the update applies to the \
                     current plan.",
                    after.id
                )));
            }
        }
        self.save(after)
    }
}

/// A held exclusive lock plus the pinned ranch directory it protects.
pub struct RanchLock {
    store: RanchDir,
    _lock: std::fs::File,
}

impl RanchLock {
    /// Validate `id` before touching the filesystem, then acquire the ranch lock.
    pub fn acquire(root: &Path, id: &str) -> Result<Self> {
        use std::os::fd::AsRawFd;

        if !is_safe_id(id) {
            return Err(Error::Invalid(format!(
                "unsafe ranch id {id:?}: must be a single path component"
            )));
        }
        let store = RanchDir::open(root, id, true)?;
        let lock = store.dir.open_regular_create(".lock", 0o644)?;
        // SAFETY: `lock` owns a valid descriptor. The lock is released on close.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Error::Invalid(format!(
                "acquiring ranch lock: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self { store, _lock: lock })
    }

    pub fn load(&self) -> Result<Ranch> {
        self.store.load()
    }

    pub fn save(&self, ranch: &Ranch) -> Result<()> {
        self.store.save(ranch)
    }

    pub fn save_progress(&self, before: &Ranch, after: &Ranch) -> Result<()> {
        self.store.save_progress(before, after)
    }

    /// Open/create the artifact parent beneath this pinned ranch directory.
    pub fn artifacts_dir(&self) -> Result<crate::fs::Dir> {
        self.store.dir.ensure_dir("artifacts")
    }
}

/// Load a ranch plan by id without following Ranch-store symlinks.
pub fn load(root: &Path, id: &str) -> Result<Ranch> {
    RanchDir::open(root, id, false)?.load()
}

/// Write a ranch plan (creates its directory and atomically renames the file).
///
/// Use [`save_progress`] for any write that is *not* meant to change the plan's scope.
pub fn save(root: &Path, ranch: &Ranch) -> Result<()> {
    RanchDir::open(root, &ranch.id, true)?.save(ranch)
}

/// Descriptor-relative raw YAML write used by the commented `ranch create` skeleton.
pub fn save_yaml(root: &Path, id: &str, yaml: &[u8]) -> Result<()> {
    RanchDir::open(root, id, true)?.write_yaml(yaml)
}

/// Write a plan whose **scope has not changed**, refusing the write if it has.
pub fn save_progress(root: &Path, before: &Ranch, after: &Ranch) -> Result<()> {
    RanchDir::open(root, &after.id, true)?.save_progress(before, after)
}

/// List all ranch plans for a project (sorted by id).
pub fn list(root: &Path) -> Vec<Ranch> {
    let Ok(root_dir) = crate::fs::Dir::open(root) else {
        return Vec::new();
    };
    let Ok(ranches_dir) = root_dir
        .open_dir(".cowboy")
        .and_then(|dir| dir.open_dir("ranches"))
    else {
        return Vec::new();
    };
    let mut ranches = ranches_dir
        .list_names()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter_map(|id| load(root, &id).ok())
        .collect::<Vec<_>>();
    ranches.sort_by(|a, b| a.id.cmp(&b.id));
    ranches
}

/// A free id from a title (slug), suffixed until unused under `root`.
pub fn fresh_id(root: &Path, title: &str) -> String {
    let base = crate::memory::slugify(title);
    let existing = crate::fs::Dir::open(root)
        .and_then(|dir| dir.open_dir(".cowboy"))
        .and_then(|dir| dir.open_dir("ranches"));
    let mut id = base.clone();
    let mut n = 2;
    while existing
        .as_ref()
        .ok()
        .and_then(|dir| dir.entry_kind(&id).ok())
        .flatten()
        .is_some()
    {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: &str, deps: &[&str], status: WorkstreamStatus) -> Workstream {
        Workstream {
            id: id.into(),
            title: id.into(),
            goal: String::new(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            status,
            session_id: None,
            branch: None,
            worktree_path: None,
            expected_artifacts: vec![],
            acceptance: vec![],
        }
    }

    fn ranch(ws: Vec<Workstream>) -> Ranch {
        Ranch {
            version: 1,
            id: "r".into(),
            title: "R".into(),
            goal: String::new(),
            status: RanchStatus::Planning,
            workstreams: ws,
            auto_advance: true,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    #[test]
    fn readiness_follows_the_dependency_graph() {
        // schema (done) -> api -> ui ; integration depends on all.
        let mut r = ranch(vec![
            ws("schema", &[], WorkstreamStatus::Complete),
            ws("api", &["schema"], WorkstreamStatus::Planned),
            ws("ui", &["api"], WorkstreamStatus::Planned),
            ws(
                "integration",
                &["schema", "api", "ui"],
                WorkstreamStatus::Planned,
            ),
        ]);
        let newly = r.recompute_readiness();
        assert!(
            newly.contains(&"api".to_string()),
            "api unblocks once schema is done"
        );
        assert_eq!(r.workstream("api").unwrap().status, WorkstreamStatus::Ready);
        assert_eq!(
            r.workstream("ui").unwrap().status,
            WorkstreamStatus::Blocked
        );
        assert_eq!(
            r.workstream("integration").unwrap().status,
            WorkstreamStatus::Blocked
        );
        let ready: Vec<_> = r.ready_workstreams().iter().map(|w| w.id.clone()).collect();
        assert_eq!(ready, vec!["api"]);
    }

    #[test]
    fn validate_catches_cycles_dangling_and_dupes() {
        // A valid linear graph passes.
        assert!(ranch(vec![
            ws("a", &[], WorkstreamStatus::Planned),
            ws("b", &["a"], WorkstreamStatus::Planned),
        ])
        .validate()
        .is_ok());

        // Dangling dependency id.
        assert!(ranch(vec![ws("a", &["nope"], WorkstreamStatus::Planned)])
            .validate()
            .is_err());

        // Cycle a -> b -> a (would otherwise silently block both forever).
        assert!(ranch(vec![
            ws("a", &["b"], WorkstreamStatus::Planned),
            ws("b", &["a"], WorkstreamStatus::Planned),
        ])
        .validate()
        .is_err());

        // Duplicate ids.
        assert!(ranch(vec![
            ws("a", &[], WorkstreamStatus::Planned),
            ws("a", &[], WorkstreamStatus::Planned),
        ])
        .validate()
        .is_err());
    }

    /// Ids from the committed (agent-writable) ranch.yaml become path components
    /// host-side and reach `remove_dir_all`/copy in the coordinator, so a traversing
    /// id must be refused before it can escape the store.
    #[test]
    fn is_safe_id_rejects_traversal() {
        assert!(is_safe_id("api"));
        assert!(is_safe_id("api-v2_3"));
        for bad in ["", ".", "..", "../..", "a/b", "/etc", "/", "a/../b", "."] {
            assert!(!is_safe_id(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn validate_rejects_traversing_ranch_and_workstream_ids() {
        // A traversing workstream id is refused (this id would flow into
        // ranch_artifact_dir -> remove_dir_all).
        let r = ranch(vec![ws("../../etc", &[], WorkstreamStatus::Planned)]);
        assert!(
            r.validate().is_err(),
            "traversing workstream id must fail validate"
        );

        // A traversing ranch id is refused too.
        let mut r = ranch(vec![ws("a", &[], WorkstreamStatus::Planned)]);
        r.id = "../../..".into();
        assert!(
            r.validate().is_err(),
            "traversing ranch id must fail validate"
        );

        // And `save` refuses it even without going through validate.
        let tmp = std::env::temp_dir().join(format!("cowboy-ranch-test-{}", std::process::id()));
        assert!(
            save(&tmp, &r).is_err(),
            "save must refuse a traversing ranch id"
        );
    }

    #[test]
    fn lock_rejects_an_unsafe_id_before_opening_the_root() {
        let root = std::env::temp_dir().join(format!(
            "cowboy-ranch-invalid-lock-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        assert!(RanchLock::acquire(&root, "../escape").is_err());
        assert!(
            !root.exists(),
            "validation must precede every path operation"
        );
    }

    #[test]
    fn ranch_store_refuses_symlinked_ancestors_lock_and_plan() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "cowboy-ranch-links-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let victim = root.with_extension("victim");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&victim).ok();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&victim).unwrap();
        symlink(&victim, root.join(".cowboy")).unwrap();
        assert!(save(&root, &ranch(vec![])).is_err());
        assert!(!victim.join("ranches").exists());

        std::fs::remove_file(root.join(".cowboy")).unwrap();
        let ranch_dir = root.join(".cowboy/ranches/r");
        std::fs::create_dir_all(&ranch_dir).unwrap();
        let victim_file = victim.join("file");
        std::fs::write(&victim_file, "untouched").unwrap();
        symlink(&victim_file, ranch_dir.join(".lock")).unwrap();
        assert!(RanchLock::acquire(&root, "r").is_err());
        assert_eq!(std::fs::read_to_string(&victim_file).unwrap(), "untouched");

        std::fs::remove_file(ranch_dir.join(".lock")).unwrap();
        symlink(&victim_file, ranch_dir.join("ranch.yaml")).unwrap();
        assert!(load(&root, "r").is_err());
        assert_eq!(std::fs::read_to_string(&victim_file).unwrap(), "untouched");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&victim).ok();
    }

    #[test]
    fn ranch_plan_fifo_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::time::Duration;

        let root = std::env::temp_dir().join(format!(
            "cowboy-ranch-fifo-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        let ranch_dir = root.join(".cowboy/ranches/r");
        std::fs::create_dir_all(&ranch_dir).unwrap();
        let plan = ranch_dir.join("ranch.yaml");
        let plan = CString::new(plan.as_os_str().as_bytes()).unwrap();
        // SAFETY: `plan` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(plan.as_ptr(), 0o600) }, 0);

        let load_root = root.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(load(&load_root, "r").is_err());
        });
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("loading a FIFO plan must not block"),
            "FIFO plan must be rejected as non-regular"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The artifact dir for any *validated* ranch stays inside the store — a
    /// traversing id can never reach this helper because load/validate reject it.
    #[test]
    fn artifact_dir_of_a_safe_id_stays_in_the_store() {
        let root = Path::new("/srv/proj");
        let dir = ranch_artifact_dir(root, "myranch", "api");
        assert!(dir.starts_with(ranches_dir(root)));
        assert_eq!(dir, root.join(".cowboy/ranches/myranch/artifacts/api"),);
    }

    /// Progress writes go through; scope writes on a progress path do not.
    ///
    /// This is the AGENTS.md rule made mechanical: the daemon coordinator and
    /// `ranch complete/accept/retry` maintain status, session ids, branches and
    /// worktrees on their own, but which workstreams exist and what they must deliver
    /// only changes when the user approves a proposal. Previously nothing checked it.
    #[test]
    fn a_progress_write_may_not_change_the_plans_scope() {
        let dir = std::env::temp_dir().join(format!("cowboy-ranch-scope-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut before = ranch(vec![ws("schema", &[], WorkstreamStatus::Planned)]);
        before.workstreams[0].acceptance = vec!["migrations apply cleanly".into()];
        before.workstreams[0].expected_artifacts = vec!["schema.sql".into()];
        save(&dir, &before).unwrap();

        // Bookkeeping: status, session, branch, worktree, timestamps, auto_advance.
        let mut progress = before.clone();
        progress.status = RanchStatus::Running;
        progress.auto_advance = !before.auto_advance;
        progress.updated_ms = 12_345;
        {
            let w = progress.workstream_mut("schema").unwrap();
            w.status = WorkstreamStatus::Running;
            w.session_id = Some("s1".into());
            w.branch = Some("cowboy/schema".into());
            w.worktree_path = Some(PathBuf::from("/w/schema"));
        }
        save_progress(&dir, &before, &progress).expect("progress must be writable");
        assert_eq!(load(&dir, "r").unwrap().status, RanchStatus::Running);

        // Scope: a new workstream, a changed dependency, a reworded goal, a dropped
        // acceptance criterion. Each must be refused on this path.
        let mut added = progress.clone();
        added
            .workstreams
            .push(ws("api", &["schema"], WorkstreamStatus::Planned));
        let mut redirected = progress.clone();
        redirected.workstreams[0].depends_on = vec!["nonexistent".into()];
        let mut regoaled = progress.clone();
        regoaled.workstreams[0].goal = "something else entirely".into();
        let mut deaccepted = progress.clone();
        deaccepted.workstreams[0].acceptance.clear();
        let mut retitled = progress.clone();
        retitled.title = "a different plan".into();

        for (label, candidate) in [
            ("a new workstream", added),
            ("a changed dependency", redirected),
            ("a reworded goal", regoaled),
            ("a dropped acceptance criterion", deaccepted),
            ("a retitled plan", retitled),
        ] {
            let err = match save_progress(&dir, &progress, &candidate) {
                Err(e) => e.to_string(),
                Ok(()) => panic!("{label} is a scope change and must be refused"),
            };
            assert!(err.contains("scope changed"), "{label}: {err}");
        }
        // And nothing leaked to disk: the committed plan still has one workstream.
        assert_eq!(load(&dir, "r").unwrap().workstreams.len(), 1);
    }

    /// M7: a progress write must also see a scope change that landed *on disk* since
    /// the caller loaded — otherwise a progress update built on the stale scope
    /// silently clobbers a concurrently-approved scope change (lost update).
    #[test]
    fn a_progress_write_refuses_a_scope_change_that_landed_on_disk() {
        let dir = std::env::temp_dir().join(format!(
            "cowboy-ranch-m7-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let before = ranch(vec![ws("schema", &[], WorkstreamStatus::Planned)]);
        save(&dir, &before).unwrap();

        // A user-gated approval lands a NEW scope on disk (adds a workstream).
        let mut approved = before.clone();
        approved
            .workstreams
            .push(ws("api", &["schema"], WorkstreamStatus::Planned));
        save(&dir, &approved).unwrap();

        // Meanwhile a progress writer that loaded the OLD scope tries to record
        // bookkeeping. Its own before→after scope is unchanged (so check 1 passes),
        // but the on-disk scope has moved on — check 2 must refuse it.
        let mut progress = before.clone();
        progress.status = RanchStatus::Running;
        let err = save_progress(&dir, &before, &progress)
            .expect_err("a progress write over a landed scope change must be refused");
        assert!(
            err.to_string().contains("changed on disk"),
            "expected an on-disk-drift refusal: {err}"
        );

        // The approved scope survives untouched — the lost update did not happen.
        let on_disk = load(&dir, "r").unwrap();
        assert_eq!(
            on_disk.workstreams.len(),
            2,
            "the approval must not be clobbered"
        );

        // And a progress write built on the CURRENT on-disk scope still works.
        let mut ok = approved.clone();
        ok.status = RanchStatus::Running;
        save_progress(&dir, &approved, &ok).expect("progress on the current scope is fine");
        assert_eq!(load(&dir, "r").unwrap().status, RanchStatus::Running);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("cowboy-ranch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = ranch(vec![ws("schema", &[], WorkstreamStatus::Planned)]);
        save(&dir, &r).unwrap();
        let back = load(&dir, "r").unwrap();
        assert_eq!(back, r);
        assert_eq!(list(&dir).len(), 1);
        // A fresh id avoids the existing one.
        assert_ne!(fresh_id(&dir, "R"), "r");
    }
}
