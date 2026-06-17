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
