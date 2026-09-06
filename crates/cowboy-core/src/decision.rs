//! Durable decision records.
//!
//! When a session asks the user to decide something (a structured `ask_user`,
//! or a Ranch scope-change resolution), the question, options, and the chosen
//! answer are recorded as a [`Decision`] — one JSON line in `decisions.jsonl`
//! under the session dir — so the rationale survives and downstream workstreams
//! can depend on it. Testable via the `*_in(session_dir, …)` functions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A recorded decision and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// Short stable id (`d0001`).
    pub id: String,
    pub session_id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    pub created_ms: u64,
}

fn path(session_dir: &Path) -> PathBuf {
    session_dir.join("decisions.jsonl")
}

/// All recorded decisions for a session, in order (absent/empty → []).
pub fn list_in(session_dir: &Path) -> Vec<Decision> {
    let Ok(text) = std::fs::read_to_string(path(session_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Decision>(l).ok())
        .collect()
}

/// One decision by id.
pub fn get_in(session_dir: &Path, id: &str) -> Option<Decision> {
    list_in(session_dir).into_iter().find(|d| d.id == id)
}

/// Record a decision (assigns the next `dNNNN` id and appends it). Best-effort
/// on write; returns the record regardless so callers can reference its id.
pub fn record_in(
    session_dir: &Path,
    session_id: &str,
    question: &str,
    options: Vec<String>,
    selected: Option<String>,
    rationale: Option<String>,
    now_ms: u64,
) -> Decision {
    // Next id from max existing id + 1, not the count: `list_in` drops unparseable
    // lines, so a count would collide on a corrupt line and overwrite a prior
    // decision. (Same fix as artifact ids.)
    let seq = list_in(session_dir)
        .iter()
        .filter_map(|d| d.id.strip_prefix('d').and_then(|n| n.parse::<u32>().ok()))
        .max()
        .map_or(1, |m| m + 1);
    let d = Decision {
        id: format!("d{seq:04}"),
        session_id: session_id.to_string(),
        question: question.to_string(),
        options,
        selected,
        rationale,
        created_ms: now_ms,
    };
    if let Ok(line) = serde_json::to_string(&d) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path(session_dir))
        {
            let _ = writeln!(f, "{line}");
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_list_and_get() {
        let dir = std::env::temp_dir().join(format!("cowboy-decision-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(path(&dir));

        let d = record_in(
            &dir,
            "s1",
            "UUIDs or sequential ids?",
            vec!["uuid".into(), "sequential".into()],
            Some("uuid".into()),
            Some("global uniqueness".into()),
            1000,
        );
        assert_eq!(d.id, "d0001");
        let d2 = record_in(&dir, "s1", "REST or GraphQL?", vec![], None, None, 1001);
        assert_eq!(d2.id, "d0002");

        let all = list_in(&dir);
        assert_eq!(all.len(), 2);
        assert_eq!(
            get_in(&dir, "d0001").unwrap().selected.as_deref(),
            Some("uuid")
        );
        assert!(get_in(&dir, "nope").is_none());
    }

    /// A corrupt index line must not make the next decision id collide with an
    /// existing one (M8, same fix as artifacts).
    #[test]
    fn a_corrupt_line_does_not_duplicate_a_decision_id() {
        let dir = std::env::temp_dir().join(format!(
            "cowboy-decision-m8-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(path(&dir));

        record_in(&dir, "s", "q1", vec![], None, None, 1);
        record_in(&dir, "s", "q2", vec![], None, None, 2);
        // Append a garbage line.
        let mut text = std::fs::read_to_string(path(&dir)).unwrap();
        text.push_str("not json at all\n");
        std::fs::write(path(&dir), &text).unwrap();
        assert_eq!(list_in(&dir).len(), 2, "bad line dropped");

        let d3 = record_in(&dir, "s", "q3", vec![], None, None, 3);
        assert_eq!(d3.id, "d0003", "next id must not collide");
        std::fs::remove_dir_all(&dir).ok();
    }
}
