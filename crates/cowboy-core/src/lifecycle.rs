//! Semantic session lifecycle events.
//!
//! Distinct from the UI event journal (`events.jsonl`, display-oriented), this
//! is the *machine-readable* stream that a Ranch coordinator and the
//! session-to-session message bus consume to react to what a session is doing:
//! it started, advanced a plan step, published an artifact, got blocked,
//! recorded a decision, finished. One JSON object per line in `lifecycle.jsonl`.
//!
//! `*_in(session_dir, …)` takes the session directory explicitly so it's
//! testable without a live session; the clock (`now_ms`) is injected.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A semantic thing that happened in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LifecycleEvent {
    SessionStarted,
    PlanStepStarted {
        step: String,
    },
    PlanStepCompleted {
        step: String,
    },
    ArtifactPublished {
        artifact_id: String,
        kind: String,
    },
    Blocked {
        reason: String,
        waiting_on: Vec<String>,
    },
    Unblocked,
    DecisionRequested {
        question: String,
    },
    DecisionRecorded {
        decision_id: String,
    },
    HandoffCreated {
        artifact_id: String,
    },
    MergeReady,
    SessionCompleted {
        status: String,
    },
}

/// A timestamped lifecycle record (one `lifecycle.jsonl` line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRecord {
    pub ts_ms: u64,
    pub session_id: String,
    pub event: LifecycleEvent,
}

fn path(session_dir: &Path) -> PathBuf {
    session_dir.join("lifecycle.jsonl")
}

/// Append a lifecycle event (best-effort; never fails the caller).
pub fn append_in(session_dir: &Path, session_id: &str, event: LifecycleEvent, now_ms: u64) {
    use std::io::Write;
    let rec = LifecycleRecord {
        ts_ms: now_ms,
        session_id: session_id.to_string(),
        event,
    };
    let Ok(line) = serde_json::to_string(&rec) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path(session_dir))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Read a session's lifecycle records in order (absent/empty → []).
pub fn read_in(session_dir: &Path) -> Vec<LifecycleRecord> {
    let Ok(text) = std::fs::read_to_string(path(session_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<LifecycleRecord>(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read_roundtrips_in_order() {
        let dir = std::env::temp_dir().join(format!("cowboy-lifecycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(path(&dir));

        append_in(&dir, "s1", LifecycleEvent::SessionStarted, 1);
        append_in(
            &dir,
            "s1",
            LifecycleEvent::PlanStepStarted {
                step: "build".into(),
            },
            2,
        );
        append_in(
            &dir,
            "s1",
            LifecycleEvent::ArtifactPublished {
                artifact_id: "a0001".into(),
                kind: "contract".into(),
            },
            3,
        );
        append_in(
            &dir,
            "s1",
            LifecycleEvent::SessionCompleted {
                status: "complete".into(),
            },
            4,
        );

        let recs = read_in(&dir);
        assert_eq!(recs.len(), 4);
        assert_eq!(recs[0].event, LifecycleEvent::SessionStarted);
        assert_eq!(recs[3].ts_ms, 4);
        assert!(matches!(
            recs[2].event,
            LifecycleEvent::ArtifactPublished { .. }
        ));
    }
}
