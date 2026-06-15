//! Generic session artifacts: typed, titled outputs a session produces
//! (contracts, summaries, handoffs, reviews, test results, …).
//!
//! Artifacts are the backbone of artifact-driven coordination: one session
//! publishes `schema-contract.md`, another consumes it. They live under the
//! producing session's directory — bytes at `artifacts/<name>` and an
//! append-only index at `artifacts.jsonl` (one [`ArtifactRef`] per line). A
//! Ranch later *promotes* a session artifact by copying it into the ranch's
//! shared store and recording the ref there too.
//!
//! The `*_in(session_dir, …)` functions take the session directory explicitly
//! so they're testable without a live session.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// What kind of artifact this is (drives default extension + display).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Contract,
    Summary,
    Patch,
    Diff,
    TestResult,
    DecisionRecord,
    Handoff,
    Review,
    Notes,
    Other,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Contract => "contract",
            ArtifactKind::Summary => "summary",
            ArtifactKind::Patch => "patch",
            ArtifactKind::Diff => "diff",
            ArtifactKind::TestResult => "test_result",
            ArtifactKind::DecisionRecord => "decision_record",
            ArtifactKind::Handoff => "handoff",
            ArtifactKind::Review => "review",
            ArtifactKind::Notes => "notes",
            ArtifactKind::Other => "other",
        }
    }

    /// Lenient parse (unknown values become [`ArtifactKind::Other`]).
    pub fn parse(s: &str) -> ArtifactKind {
        match s.trim().to_lowercase().replace(['-', ' '], "_").as_str() {
            "contract" => ArtifactKind::Contract,
            "summary" => ArtifactKind::Summary,
            "patch" => ArtifactKind::Patch,
            "diff" => ArtifactKind::Diff,
            "test_result" | "tests" | "test" => ArtifactKind::TestResult,
            "decision_record" | "decision" => ArtifactKind::DecisionRecord,
            "handoff" => ArtifactKind::Handoff,
            "review" => ArtifactKind::Review,
            "notes" | "note" => ArtifactKind::Notes,
            _ => ArtifactKind::Other,
        }
    }

    /// File extension for a freshly written artifact of this kind.
    fn ext(self) -> &'static str {
        match self {
            ArtifactKind::Patch => "patch",
            ArtifactKind::Diff => "diff",
            _ => "md",
        }
    }
}

/// An index entry describing one stored artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Short stable id (the zero-padded sequence, e.g. `a0001`).
    pub id: String,
    pub session_id: String,
    pub kind: ArtifactKind,
    pub title: String,
    /// Path to the bytes, relative to the session directory.
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub created_ms: u64,
}

fn index_path(session_dir: &Path) -> PathBuf {
    session_dir.join("artifacts.jsonl")
}

/// All artifacts recorded for a session, in publish order (absent/empty → []).
pub fn list_in(session_dir: &Path) -> Vec<ArtifactRef> {
    let Ok(text) = std::fs::read_to_string(index_path(session_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ArtifactRef>(l).ok())
        .collect()
}

/// The ref + body of one artifact by id.
pub fn get_in(session_dir: &Path, id: &str) -> Option<(ArtifactRef, String)> {
    let r = list_in(session_dir).into_iter().find(|a| a.id == id)?;
    let body = std::fs::read_to_string(session_dir.join(&r.path)).ok()?;
    Some((r, body))
}

/// Publish an artifact: write its bytes under `artifacts/` and append a ref to
/// the index. `now_ms` is injected so callers (and tests) control the clock.
pub fn add_in(
    session_dir: &Path,
    session_id: &str,
    kind: ArtifactKind,
    title: &str,
    content: &str,
    summary: Option<String>,
    now_ms: u64,
) -> Result<ArtifactRef> {
    let seq = list_in(session_dir).len() + 1;
    let id = format!("a{seq:04}");
    let stem = crate::memory::slugify(title);
    let rel = PathBuf::from("artifacts").join(format!("{id}-{stem}.{}", kind.ext()));

    let abs = session_dir.join(&rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Invalid(e.to_string()))?;
    }
    std::fs::write(&abs, content).map_err(|e| Error::Invalid(e.to_string()))?;

    let r = ArtifactRef {
        id,
        session_id: session_id.to_string(),
        kind,
        title: title.to_string(),
        path: rel,
        summary,
        created_ms: now_ms,
    };
    append_ref(session_dir, &r)?;
    Ok(r)
}

fn append_ref(session_dir: &Path, r: &ArtifactRef) -> Result<()> {
    use std::io::Write;
    let line = serde_json::to_string(r).map_err(|e| Error::Invalid(e.to_string()))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path(session_dir))
        .map_err(|e| Error::Invalid(e.to_string()))?;
    writeln!(f, "{line}").map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cowboy-artifact-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn add_list_get_roundtrip() {
        let dir = tmp();
        let a = add_in(
            &dir,
            "sess1",
            ArtifactKind::Contract,
            "API Contract",
            "# API\nGET /things\n",
            Some("the billing API surface".into()),
            1000,
        )
        .unwrap();
        assert_eq!(a.id, "a0001");
        assert_eq!(a.path, PathBuf::from("artifacts/a0001-api-contract.md"));

        let b = add_in(
            &dir,
            "sess1",
            ArtifactKind::Diff,
            "the diff",
            "patch\n",
            None,
            1001,
        )
        .unwrap();
        assert_eq!(b.id, "a0002");
        assert!(b.path.to_string_lossy().ends_with(".diff"));

        let listed = list_in(&dir);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].title, "API Contract");

        let (got, body) = get_in(&dir, "a0001").unwrap();
        assert_eq!(got.kind, ArtifactKind::Contract);
        assert!(body.contains("GET /things"));
        assert!(get_in(&dir, "nope").is_none());
    }

    #[test]
    fn kind_parse_is_lenient() {
        assert_eq!(ArtifactKind::parse("Contract"), ArtifactKind::Contract);
        assert_eq!(ArtifactKind::parse("test-result"), ArtifactKind::TestResult);
        assert_eq!(ArtifactKind::parse("whatever"), ArtifactKind::Other);
    }

    #[test]
    fn missing_index_is_empty() {
        assert!(list_in(&tmp()).is_empty());
    }
}
