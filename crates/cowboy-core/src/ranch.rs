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

/// Load a ranch plan by id.
pub fn load(root: &Path, id: &str) -> Result<Ranch> {
    let path = ranch_path(root, id);
    let text = std::fs::read_to_string(&path)
        .map_err(|_| Error::Invalid(format!("no ranch `{id}` ({})", path.display())))?;
    serde_yaml_ng::from_str(&text).map_err(|e| Error::Invalid(format!("parsing {id}: {e}")))
}

/// Write a ranch plan (creates its dir; atomic temp+rename).
///
/// Use [`save_progress`] for any write that is *not* meant to change the plan's scope.
pub fn save(root: &Path, ranch: &Ranch) -> Result<()> {
    let path = ranch_path(root, &ranch.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Invalid(e.to_string()))?;
    }
    let yaml = serde_yaml_ng::to_string(ranch).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml).map_err(|e| Error::Invalid(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

/// Write a plan whose **scope has not changed**, refusing the write if it has.
///
/// `ranch.yaml` is the committed source of truth, and the rule is that its *scope* —
/// which workstreams exist, what they depend on, what they are for, what they must
/// deliver — changes only when the user says so, via a scope proposal and
/// `cowboy ranch approve`. Progress is different: which workstream is running, its
/// session id, branch and worktree, and the derived overall status. The daemon
/// coordinator writes those on its own, all day.
///
/// That distinction was documented and then enforced by nothing, which is the kind of
/// invariant that quietly stops being true. `before` is the plan as loaded; passing it
/// here makes the write assert what it claims. The check is on the scope fields only,
/// so ordinary bookkeeping passes and an accidental (or agent-driven) scope edit on a
/// progress path fails loudly instead of landing in a committed file.
pub fn save_progress(root: &Path, before: &Ranch, after: &Ranch) -> Result<()> {
    if before.scope_fingerprint() != after.scope_fingerprint() {
        return Err(Error::Invalid(format!(
            "refusing to write ranch `{}`: this is a progress update, but the plan's scope \
             changed. Scope changes go through a proposal and `cowboy ranch approve`",
            after.id
        )));
    }
    save(root, after)
}

/// List all ranch plans for a project (newest activity is not implied; sorted by id).
pub fn list(root: &Path) -> Vec<Ranch> {
    let mut ranches = Vec::new();
    if let Ok(entries) = std::fs::read_dir(ranches_dir(root)) {
        for e in entries.flatten() {
            if let Some(id) = e.file_name().to_str() {
                if let Ok(r) = load(root, id) {
                    ranches.push(r);
                }
            }
        }
    }
    ranches.sort_by(|a, b| a.id.cmp(&b.id));
    ranches
}

/// A free id from a title (slug), suffixed until unused under `root`.
pub fn fresh_id(root: &Path, title: &str) -> String {
    let base = crate::memory::slugify(title);
    let mut id = base.clone();
    let mut n = 2;
    while ranches_dir(root).join(&id).exists() {
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
