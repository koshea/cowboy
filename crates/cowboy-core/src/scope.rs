//! Scope-change proposals: the gated path for changing a ranch plan.
//!
//! The committed `ranch.yaml` is the source of truth and is **never** edited by
//! a worker agent (nor autonomously by the coordinator). When the plan looks
//! wrong — a missing workstream, an unnecessary one, a concern worth raising —
//! the change is filed as a *proposal* that a human reviews and approves or
//! rejects. Only on approval is `ranch.yaml` modified. Proposals live at
//! `.cowboy/ranches/<id>/proposals/<pid>.yaml` (committed: a shareable audit
//! trail of how the plan evolved).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ranch::{self, Workstream};

/// A concrete, applicable change to a ranch plan. `Note` carries no structured
/// change — it's a concern/suggestion recorded for the human (approving it just
/// acknowledges it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScopeChange {
    /// Add a new workstream to the plan.
    AddWorkstream { workstream: Workstream },
    /// Remove a workstream (only allowed if it hasn't started / isn't done).
    RemoveWorkstream { id: String },
    /// A free-form suggestion or concern (no automatic edit).
    Note,
}

impl ScopeChange {
    /// A short human label for listings.
    pub fn label(&self) -> String {
        match self {
            ScopeChange::AddWorkstream { workstream } => {
                format!("add workstream `{}`", workstream.id)
            }
            ScopeChange::RemoveWorkstream { id } => format!("remove workstream `{id}`"),
            ScopeChange::Note => "note".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
}

/// A proposed change awaiting (or having received) a human decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeProposal {
    pub id: String,
    pub ranch_id: String,
    /// Who raised it: `"user"`, or a workstream/session id.
    #[serde(default)]
    pub from: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    pub change: ScopeChange,
    #[serde(default = "default_pending")]
    pub status: ProposalStatus,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
}

fn default_pending() -> ProposalStatus {
    ProposalStatus::Pending
}

// ---------------------------------------------------------------------------
// Storage  (.cowboy/ranches/<id>/proposals/<pid>.yaml — committed)
// ---------------------------------------------------------------------------

/// The proposals directory for a ranch.
pub fn proposals_dir(root: &Path, ranch_id: &str) -> PathBuf {
    ranch::ranches_dir(root).join(ranch_id).join("proposals")
}

/// The file for one proposal.
pub fn proposal_path(root: &Path, ranch_id: &str, pid: &str) -> PathBuf {
    proposals_dir(root, ranch_id).join(format!("{pid}.yaml"))
}

/// Write a proposal (creates its dir; atomic temp+rename).
pub fn save(root: &Path, p: &ScopeProposal) -> Result<()> {
    let path = proposal_path(root, &p.ranch_id, &p.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Invalid(e.to_string()))?;
    }
    let yaml = serde_yaml_ng::to_string(p).map_err(|e| Error::Invalid(e.to_string()))?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml).map_err(|e| Error::Invalid(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

/// Load one proposal by id.
pub fn load(root: &Path, ranch_id: &str, pid: &str) -> Result<ScopeProposal> {
    let path = proposal_path(root, ranch_id, pid);
    let text = std::fs::read_to_string(&path)
        .map_err(|_| Error::Invalid(format!("no proposal `{pid}` for ranch `{ranch_id}`")))?;
    serde_yaml_ng::from_str(&text).map_err(|e| Error::Invalid(format!("parsing {pid}: {e}")))
}

/// List all proposals for a ranch, sorted by id.
pub fn list(root: &Path, ranch_id: &str) -> Vec<ScopeProposal> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(proposals_dir(root, ranch_id)) {
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(pid) = name.strip_suffix(".yaml") {
                if let Ok(p) = load(root, ranch_id, pid) {
                    out.push(p);
                }
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// A fresh sequential proposal id (`p0001`, `p0002`, …) unused under `root`.
pub fn fresh_id(root: &Path, ranch_id: &str) -> String {
    let dir = proposals_dir(root, ranch_id);
    for n in 1.. {
        let id = format!("p{n:04}");
        if !dir.join(format!("{id}.yaml")).exists() {
            return id;
        }
    }
    unreachable!("the sequence always finds a free id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ranch::WorkstreamStatus;

    fn proposal(id: &str, change: ScopeChange) -> ScopeProposal {
        ScopeProposal {
            id: id.into(),
            ranch_id: "billing".into(),
            from: "user".into(),
            summary: "s".into(),
            rationale: None,
            change,
            status: ProposalStatus::Pending,
            created_ms: 1,
            decided_ms: None,
            decision_reason: None,
        }
    }

    #[test]
    fn save_load_list_roundtrips_and_ids_are_sequential() {
        let dir = std::env::temp_dir().join(format!("cowboy-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.as_path();
        let p1 = proposal(&fresh_id(root, "billing"), ScopeChange::Note);
        save(root, &p1).unwrap();
        assert_eq!(p1.id, "p0001");

        let w = Workstream {
            id: "cache".into(),
            title: "Cache".into(),
            goal: "add caching".into(),
            depends_on: vec!["api".into()],
            status: WorkstreamStatus::Planned,
            session_id: None,
            branch: None,
            worktree_path: None,
            expected_artifacts: vec![],
            acceptance: vec![],
        };
        let p2 = proposal(
            &fresh_id(root, "billing"),
            ScopeChange::AddWorkstream { workstream: w },
        );
        save(root, &p2).unwrap();
        assert_eq!(p2.id, "p0002");

        let back = load(root, "billing", "p0002").unwrap();
        assert_eq!(back, p2);
        let all = list(root, "billing");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "p0001");
        assert_eq!(all[1].change.label(), "add workstream `cache`");
    }
}
