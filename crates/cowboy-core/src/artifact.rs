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

impl ArtifactRef {
    /// Whether `path` is a safe session-relative path (no absolute root, no `..`
    /// component). `add_in` always produces `artifacts/<name>`, so this only ever
    /// rejects a hand-crafted or tampered index entry — before it can `join` out of
    /// the session dir on read/promotion.
    fn has_safe_path(&self) -> bool {
        use std::path::Component;
        !self.path.as_os_str().is_empty()
            && self
                .path
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    }
}

/// Parse a complete artifact index, rejecting malformed records and unsafe paths.
///
/// Promotion uses this strict parser so a tampered index cannot publish a partial
/// snapshot. Best-effort artifact discovery remains available through [`list_in`].
pub fn parse_index(text: &str) -> Result<Vec<ArtifactRef>> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let line_number = index + 1;
            let artifact = serde_json::from_str::<ArtifactRef>(line).map_err(|error| {
                Error::Invalid(format!(
                    "parsing artifact index line {line_number}: {error}"
                ))
            })?;
            if !artifact.has_safe_path() {
                return Err(Error::Invalid(format!(
                    "artifact index line {line_number} has unsafe path {:?}",
                    artifact.path
                )));
            }
            Ok(artifact)
        })
        .collect()
}

fn index_path(session_dir: &Path) -> PathBuf {
    session_dir.join("artifacts.jsonl")
}

/// All artifacts recorded for a session, in publish order (absent/empty → []).
///
/// Entries whose `path` is not a safe session-relative path are dropped: the index
/// is a jsonl file inside the (agent-writable) session dir, and `get_in` joins `path`
/// onto that directory. A ref with `../…` or an absolute path would otherwise read a
/// host file outside the session. Ranch promotion instead uses [`parse_index`] so any
/// malformed or unsafe record fails the complete snapshot rather than narrowing it.
pub fn list_in(session_dir: &Path) -> Vec<ArtifactRef> {
    let Ok(text) = std::fs::read_to_string(index_path(session_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ArtifactRef>(l).ok())
        .filter(|a| a.has_safe_path())
        .collect()
}

/// The ref + body of one artifact by id.
pub fn get_in(session_dir: &Path, id: &str) -> Option<(ArtifactRef, String)> {
    let r = list_in(session_dir).into_iter().find(|a| a.id == id)?;
    let body = std::fs::read_to_string(session_dir.join(&r.path)).ok()?;
    Some((r, body))
}

/// The most of an artifact's body a viewer loads. The bytes are agent-written, so a
/// viewer must not be made to pull an arbitrarily large file into memory to show it.
pub const VIEW_MAX_BYTES: usize = 1 << 20;

/// The host file behind `r`, for linking or showing it to the user — or `None`
/// when it is not exactly where [`add_in`] puts artifacts.
///
/// SECURITY: the session dir is inside the workspace, which the agent can write, so
/// the index, the bytes *and the directories above them* are agent-controlled. The
/// result ends up behind a `file://` link the user clicks, and a terminal hands that
/// to `xdg-open`/`open`, which launches whatever handler the file looks like. So the
/// file must be a regular file, carry an extension `add_in` writes, and resolve —
/// every symlink followed — to exactly `session_dir/artifacts/<name>`. A symlinked
/// `artifacts/` (or `.cowboy/`, or the file itself) fails the comparison.
///
/// `session_dir` must therefore be canonical: built from a canonicalized root, as the
/// worker's is. A non-canonical one fails closed rather than open.
pub fn host_file(session_dir: &Path, r: &ArtifactRef) -> Option<PathBuf> {
    use std::path::Component;
    let mut parts = r
        .path
        .components()
        .filter(|c| !matches!(c, Component::CurDir));
    let (Some(Component::Normal(dir)), Some(Component::Normal(name)), None) =
        (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if dir != "artifacts" {
        return None;
    }
    let ext = Path::new(name).extension()?.to_str()?;
    if !matches!(ext, "md" | "patch" | "diff") {
        return None;
    }
    let expected = session_dir.join("artifacts").join(name);
    if !std::fs::symlink_metadata(&expected)
        .ok()?
        .file_type()
        .is_file()
    {
        return None;
    }
    (std::fs::canonicalize(&expected).ok()? == expected).then_some(expected)
}

/// `path` as a `file://` URL, percent-encoded so the result is one token with no
/// spaces or control characters — safe to print, to put in an OSC 8 hyperlink, and
/// for a terminal's URL detection to find in full.
pub fn file_url(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let mut url = String::from("file://");
    for b in bytes {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// Read a body for display: at most [`VIEW_MAX_BYTES`], lossily decoded, and whether
/// it was cut short.
pub fn read_for_view(path: &Path) -> Option<(String, bool)> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(VIEW_MAX_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    let truncated = buf.len() > VIEW_MAX_BYTES;
    buf.truncate(VIEW_MAX_BYTES);
    // A cut can land inside a multi-byte character; lossy decoding turns the stub
    // into one replacement character rather than failing the whole view.
    Some((String::from_utf8_lossy(&buf).into_owned(), truncated))
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
    // Next id from the max existing id + 1, NOT from the count. `list_in` silently
    // drops unparseable jsonl lines, so a single corrupt line would shrink the count
    // and hand out an id that already exists — overwriting a prior artifact's file
    // and shadowing its index entry (and ranch promotion could then feed downstream
    // workstreams the wrong bytes). Max-id+1 is monotonic regardless of gaps or a
    // bad line.
    let seq = list_in(session_dir)
        .iter()
        .filter_map(|a| a.id.strip_prefix('a').and_then(|n| n.parse::<u32>().ok()))
        .max()
        .map_or(1, |m| m + 1);
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

    /// A tampered/hand-crafted index entry whose `path` escapes the session dir
    /// (`../…` or absolute) is dropped by `list_in`, so neither `get_in` nor ranch
    /// promotion can read/copy a host file outside the session. (low finding)
    #[test]
    fn an_index_entry_with_a_traversing_path_is_dropped() {
        let dir = tmp();
        // A legitimate artifact, plus two hand-written malicious index lines.
        add_in(&dir, "s", ArtifactKind::Other, "ok", "hi", None, 1).unwrap();
        let index = dir.join("artifacts.jsonl");
        let mut text = std::fs::read_to_string(&index).unwrap();
        text.push_str(
            "{\"id\":\"a9998\",\"session_id\":\"s\",\"kind\":\"other\",\"title\":\"evil\",\
             \"path\":\"../../../../etc/passwd\",\"created_ms\":2}\n",
        );
        text.push_str(
            "{\"id\":\"a9999\",\"session_id\":\"s\",\"kind\":\"other\",\"title\":\"evil2\",\
             \"path\":\"/etc/hostname\",\"created_ms\":3}\n",
        );
        std::fs::write(&index, &text).unwrap();

        let listed = list_in(&dir);
        assert_eq!(
            listed.len(),
            1,
            "traversing/absolute entries must be filtered out"
        );
        assert_eq!(listed[0].title, "ok");
        // And they can't be fetched by id either.
        assert!(get_in(&dir, "a9998").is_none());
        assert!(get_in(&dir, "a9999").is_none());
    }

    /// A corrupt (unparseable) index line must not cause the next id to collide with
    /// an existing one. `list_in` drops the bad line, so a count-based id would reuse
    /// a live id and overwrite it; max-id+1 stays monotonic. (M8)
    #[test]
    fn a_corrupt_index_line_does_not_cause_a_duplicate_id() {
        let dir = tmp();
        let a = add_in(&dir, "s", ArtifactKind::Other, "one", "1", None, 1).unwrap();
        let b = add_in(&dir, "s", ArtifactKind::Other, "two", "2", None, 2).unwrap();
        assert_eq!(a.id, "a0001");
        assert_eq!(b.id, "a0002");

        // Corrupt the index: append a garbage line that won't parse.
        let index = dir.join("artifacts.jsonl");
        let mut text = std::fs::read_to_string(&index).unwrap();
        text.push_str("{ this is not valid json\n");
        std::fs::write(&index, &text).unwrap();
        assert_eq!(
            list_in(&dir).len(),
            2,
            "the bad line is dropped, so count is now short"
        );

        // The next id must still be a0003, not a0002 (which count+1 would produce).
        let c = add_in(&dir, "s", ArtifactKind::Other, "three", "3", None, 3).unwrap();
        assert_eq!(
            c.id, "a0003",
            "next id must not collide with an existing one"
        );
        // And the earlier artifact's file/body is intact (not overwritten).
        let (_, body) = get_in(&dir, "a0002").unwrap();
        assert_eq!(body, "2");
    }

    #[test]
    fn strict_index_parser_rejects_malformed_records_and_unsafe_paths() {
        let malformed = parse_index("{not json\n").unwrap_err();
        assert!(malformed.to_string().contains("artifact index line 1"));

        let unsafe_ref = ArtifactRef {
            id: "a0001".into(),
            session_id: "s".into(),
            kind: ArtifactKind::Other,
            title: "escape".into(),
            path: PathBuf::from("../outside"),
            summary: None,
            created_ms: 1,
        };
        let text = serde_json::to_string(&unsafe_ref).unwrap();
        let unsafe_path = parse_index(&text).unwrap_err();
        assert!(unsafe_path.to_string().contains("unsafe path"));
    }

    fn canonical_tmp() -> PathBuf {
        std::fs::canonicalize(tmp()).unwrap()
    }

    #[test]
    fn a_published_artifact_has_a_host_file_and_a_link() {
        let dir = canonical_tmp();
        let a = add_in(
            &dir,
            "s",
            ArtifactKind::Review,
            "the review",
            "# hi",
            None,
            1,
        )
        .unwrap();
        let file = host_file(&dir, &a).expect("a fresh artifact is linkable");
        assert_eq!(file, dir.join(&a.path));
        let url = file_url(&file);
        assert!(url.starts_with("file:///"), "{url}");
        assert!(url.ends_with("/artifacts/a0001-the-review.md"), "{url}");
        assert_eq!(read_for_view(&file), Some(("# hi".to_string(), false)));
    }

    #[test]
    fn a_file_url_is_one_printable_token() {
        let url = file_url(Path::new("/home/me/my proj/ü\n.md"));
        assert_eq!(url, "file:///home/me/my%20proj/%C3%BC%0A.md");
        assert!(!url.chars().any(|c| c.is_control() || c == ' '));
    }

    /// The session dir is agent-writable, and a link to it is something the user clicks
    /// and the desktop *opens*. Each of these is a way the agent could make that link
    /// launch something other than one of this session's markdown or patch files.
    #[cfg(unix)]
    #[test]
    fn host_file_refuses_anything_the_agent_could_have_redirected() {
        use std::os::unix::fs::symlink;
        let dir = canonical_tmp();
        let a = add_in(&dir, "s", ArtifactKind::Notes, "ok", "x", None, 1).unwrap();
        let reference = |path: &str| ArtifactRef {
            path: PathBuf::from(path),
            ..a.clone()
        };
        let outside = canonical_tmp().join("payload.md");
        std::fs::write(&outside, "evil").unwrap();

        // The file itself is a symlink out of the session.
        symlink(&outside, dir.join("artifacts/a0002-link.md")).unwrap();
        assert_eq!(host_file(&dir, &reference("artifacts/a0002-link.md")), None);
        // An extension add_in never writes (a launcher, a script).
        std::fs::write(dir.join("artifacts/a0003-x.desktop"), "[Desktop Entry]").unwrap();
        assert_eq!(
            host_file(&dir, &reference("artifacts/a0003-x.desktop")),
            None
        );
        // A file outside `artifacts/`, or nested below it.
        std::fs::write(dir.join("events.md"), "x").unwrap();
        assert_eq!(host_file(&dir, &reference("events.md")), None);
        std::fs::create_dir_all(dir.join("artifacts/sub")).unwrap();
        std::fs::write(dir.join("artifacts/sub/a.md"), "x").unwrap();
        assert_eq!(host_file(&dir, &reference("artifacts/sub/a.md")), None);
        // A directory with an allowed-looking name.
        std::fs::create_dir_all(dir.join("artifacts/a0004-dir.md")).unwrap();
        assert_eq!(host_file(&dir, &reference("artifacts/a0004-dir.md")), None);

        // `artifacts/` itself swapped for a symlink to somewhere else.
        let other = canonical_tmp();
        std::fs::create_dir_all(other.join("artifacts")).unwrap();
        std::fs::write(other.join("artifacts/a0001-ok.md"), "evil").unwrap();
        std::fs::rename(dir.join("artifacts"), dir.join("moved")).unwrap();
        symlink(other.join("artifacts"), dir.join("artifacts")).unwrap();
        assert_eq!(host_file(&dir, &a), None);
    }

    #[test]
    fn a_large_body_is_cut_for_viewing() {
        let dir = tmp();
        let path = dir.join("big.md");
        std::fs::write(&path, "x".repeat(VIEW_MAX_BYTES + 10)).unwrap();
        let (body, truncated) = read_for_view(&path).unwrap();
        assert!(truncated);
        assert_eq!(body.len(), VIEW_MAX_BYTES);
    }
}
