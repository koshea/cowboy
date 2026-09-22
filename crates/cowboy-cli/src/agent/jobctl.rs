//! The parent↔worker control channel for **turn requests**.
//!
//! A supervised worker that spends its turn grant does not simply stop: it writes a
//! progress report and blocks until the foreman answers. That needs a channel in both
//! directions, and the existing ones only go one way — a child's journal
//! (`events.jsonl`) is read-only advisory, and the parent's only input to a child is
//! its environment at spawn.
//!
//! **Why this lives outside the workspace.** The obvious home would be the child's
//! session directory, next to its journal. But `.cowboy/` is inside the workspace bind
//! and *writable from inside the sandbox*, so a verdict file there could be written by
//! sandboxed content and would then steer another agent's context. That is prompt
//! injection with a file for a mouth. Host-side state under `$XDG_STATE_HOME` instead,
//! the same reasoning that puts runtime grants and network approvals there rather than
//! in the project — and that moved the agent's own `HOME` out of `.cowboy/home` to
//! `~/.cache/cowboy/home/<repo-key>`.
//!
//! Both ends are host-side processes — the agent loop runs on the host and only its
//! shell commands are sandboxed — so nothing is lost by keeping this off the workspace.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Env: the control directory a worker was assigned by its parent. Absent for the
/// foreman and for any worker whose roster disabled supervision.
pub const ENV_JOB_CONTROL_DIR: &str = "COWBOY_JOB_CONTROL_DIR";

/// A worker's request for more turns, with the report the foreman judges it on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRequest {
    /// 1-based request number for this worker. Requests and verdicts are paired by it,
    /// so a stale verdict cannot answer a newer question.
    pub seq: u32,
    /// The worker's own account: what it did, what remains, what is next.
    pub report: String,
    /// Host-measured evidence (files read, edits, commands, novelty), appended so the
    /// foreman judges against measurements rather than the worker's optimism.
    pub evidence: String,
    /// How many more turns it is asking for.
    pub requested: u32,
    /// Turns spent, and the grant they were spent against.
    pub used: u32,
    pub granted: u32,
}

/// The foreman's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Keep going with `iterations` more turns.
    Grant { seq: u32, iterations: u32 },
    /// Keep going, but do this instead.
    Redirect {
        seq: u32,
        iterations: u32,
        instructions: String,
    },
    /// Stop exploring and write up what you have.
    WrapUp { seq: u32 },
    /// The work is no longer wanted.
    Stop { seq: u32, reason: String },
}

impl Verdict {
    pub fn seq(&self) -> u32 {
        match self {
            Verdict::Grant { seq, .. }
            | Verdict::Redirect { seq, .. }
            | Verdict::WrapUp { seq }
            | Verdict::Stop { seq, .. } => *seq,
        }
    }

    /// Extra turns this verdict carries. `wrap_up` grants a few so the worker can
    /// actually write its answer — ending a turn with "report now" and no turns to do
    /// it in would produce exactly the empty result this mechanism exists to prevent.
    pub fn extra_turns(&self) -> u32 {
        match self {
            Verdict::Grant { iterations, .. } | Verdict::Redirect { iterations, .. } => *iterations,
            Verdict::WrapUp { .. } => WRAP_UP_TURNS,
            Verdict::Stop { .. } => 0,
        }
    }

    /// A short word for logs and the UI.
    pub fn kind(&self) -> &'static str {
        match self {
            Verdict::Grant { .. } => "grant",
            Verdict::Redirect { .. } => "redirect",
            Verdict::WrapUp { .. } => "wrap_up",
            Verdict::Stop { .. } => "stop",
        }
    }
}

/// Turns handed to a worker told to wrap up: enough to write a report, not enough to
/// resume exploring.
///
/// Six, not three. Three was the observed failure: a worker told to wrap up spent one
/// turn writing its 17.6 KB audit, one publishing it as an artifact, one writing a
/// handoff — all correct — and hit the ceiling on the turn where it would have called
/// `final`. The foreman got a checkpoint that looked like nothing had happened and re-ran
/// the whole review. Writing up honestly costs write + publish + handoff + `final`, so
/// the floor is four; six leaves room for a split write or a retry without being enough
/// turns to start investigating again.
pub const WRAP_UP_TURNS: u32 = 6;

/// A worker's question for whoever is driving.
///
/// Separate from [`TurnRequest`] because it is a different conversation with a different
/// failure mode. A turn request is about *budget*, is always adjudicated, and times out to
/// a small extension. A question is about *the work*, may be unanswerable (nobody is
/// attached), and times out to "proceed without an answer" — which is what a subagent
/// already did, only now it can also get a real answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// 1-based, per worker. Paired with the answer so a stale answer cannot resolve a
    /// newer question.
    pub seq: u32,
    pub question: String,
    /// Suggested answers, if the worker offered any.
    #[serde(default)]
    pub options: Vec<String>,
}

/// The reply to a [`Question`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub seq: u32,
    pub answer: String,
}

/// One worker's control directory.
#[derive(Debug, Clone)]
pub struct ControlDir {
    path: PathBuf,
}

impl ControlDir {
    /// The directory for `job` under `parent`, creating it owner-only. `None` when no
    /// host-side location can be determined, which simply means supervision is
    /// unavailable — never a reason to fail a delegation.
    pub fn create(parent: &str, job: &str) -> Option<Self> {
        let path = jobs_root()?.join(sanitize(parent)).join(sanitize(job));
        std::fs::create_dir_all(&path).ok()?;
        // 0700: the verdicts here shape another agent's instructions.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        Some(Self { path })
    }

    /// Wrap an existing directory, for tests and for a worker that was handed one.
    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The directory a worker was handed in its environment.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(ENV_JOB_CONTROL_DIR).ok()?;
        let path = PathBuf::from(raw);
        path.is_dir().then_some(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Worker side: publish a request and wait to be answered.
    pub fn write_request(&self, req: &TurnRequest) -> std::io::Result<()> {
        write_private(&self.path.join(format!("request-{}.json", req.seq)), req)
    }

    /// Parent side: read the outstanding request (mostly for a client that wants the
    /// full text; the loop already has it from the job event).
    pub fn read_request(&self, seq: u32) -> Option<TurnRequest> {
        read_json(&self.path.join(format!("request-{seq}.json")))
    }

    /// Parent side: answer a request.
    pub fn write_verdict(&self, v: &Verdict) -> std::io::Result<()> {
        write_private(&self.path.join(format!("verdict-{}.json", v.seq())), v)
    }

    /// Worker side: the verdict for `seq`, if it has arrived.
    ///
    /// A verdict for a *different* sequence is ignored rather than misapplied, and a
    /// half-written or corrupt file reads as "not yet" — the caller's timeout then
    /// applies, which degrades to a small automatic extension rather than to a wedged
    /// worker.
    pub fn read_verdict(&self, seq: u32) -> Option<Verdict> {
        let v: Verdict = read_json(&self.path.join(format!("verdict-{seq}.json")))?;
        (v.seq() == seq).then_some(v)
    }

    /// Worker side: ask a question and wait to be answered.
    pub fn write_question(&self, q: &Question) -> std::io::Result<()> {
        write_private(&self.path.join(format!("question-{}.json", q.seq)), q)
    }

    /// Parent side: read an outstanding question.
    pub fn read_question(&self, seq: u32) -> Option<Question> {
        read_json(&self.path.join(format!("question-{seq}.json")))
    }

    /// Parent side: answer a question.
    pub fn write_answer(&self, a: &Answer) -> std::io::Result<()> {
        write_private(&self.path.join(format!("answer-{}.json", a.seq)), a)
    }

    /// Worker side: the answer to `seq`, if it has arrived.
    ///
    /// Sequence-checked for the same reason verdicts are: an answer to an earlier
    /// question must not be read as an answer to this one.
    pub fn read_answer(&self, seq: u32) -> Option<Answer> {
        let a: Answer = read_json(&self.path.join(format!("answer-{seq}.json")))?;
        (a.seq == seq).then_some(a)
    }

    /// Remove the directory once the job is over, and the per-session parent with it
    /// when that was the last job.
    ///
    /// Best-effort: a leftover directory is a few hundred bytes, not a correctness
    /// problem. The parent sweep matters anyway — without it every session that ever
    /// delegated leaves an empty directory behind forever.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.path);
        // Only succeeds while empty, which is exactly the condition we want.
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// `$XDG_STATE_HOME/cowboy/jobs`, else `~/.local/state/cowboy/jobs`. `None` when
/// neither is available.
fn jobs_root() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => {
            PathBuf::from(std::env::var_os("HOME").filter(|s| !s.is_empty())?).join(".local/state")
        }
    };
    Some(base.join("cowboy/jobs"))
}

/// Session ids are generated, but they arrive here as strings and are used as path
/// components, so keep them to characters that cannot escape the directory.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Write JSON 0600, flushed, via a temp file + rename so a reader never sees half a
/// message. The atomicity is the point: the reader polls, and a partial read would
/// otherwise be indistinguishable from a corrupt verdict.
///
/// The directory is (re)created first. It can legitimately be missing: `cleanup` sweeps
/// the per-session parent once its last job is gone, and a write racing that sweep would
/// otherwise fail for a reason that has nothing to do with the message.
fn write_private<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(serde_json::to_string(value)?.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A control dir in a temp location, without touching the real state dir.
    fn tmp_dir() -> (assert_fs::TempDir, ControlDir) {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("jobs/parent/job");
        std::fs::create_dir_all(&path).unwrap();
        (tmp, ControlDir { path })
    }

    fn request(seq: u32) -> TurnRequest {
        TurnRequest {
            seq,
            report: "mapped the crate; 3 files left to review".into(),
            evidence: "files read: 12 · files edited: 0".into(),
            requested: 30,
            used: 25,
            granted: 25,
        }
    }

    #[test]
    fn a_request_and_its_verdict_round_trip() {
        let (_tmp, dir) = tmp_dir();
        dir.write_request(&request(1)).unwrap();
        assert_eq!(dir.read_request(1), Some(request(1)));

        let v = Verdict::Grant {
            seq: 1,
            iterations: 30,
        };
        dir.write_verdict(&v).unwrap();
        assert_eq!(dir.read_verdict(1), Some(v));
    }

    #[test]
    fn there_is_no_verdict_until_one_is_written() {
        let (_tmp, dir) = tmp_dir();
        assert_eq!(dir.read_verdict(1), None);
    }

    #[test]
    fn a_verdict_for_another_request_is_never_misapplied() {
        // The worker asks twice; an answer to the first must not be read as an answer
        // to the second, or a `stop` could be applied to work that superseded it.
        let (_tmp, dir) = tmp_dir();
        dir.write_verdict(&Verdict::WrapUp { seq: 1 }).unwrap();
        assert!(dir.read_verdict(2).is_none());
        assert!(dir.read_verdict(1).is_some());
    }

    #[test]
    fn a_corrupt_verdict_reads_as_not_yet_rather_than_panicking() {
        let (_tmp, dir) = tmp_dir();
        std::fs::write(dir.path().join("verdict-1.json"), "{not json").unwrap();
        assert!(dir.read_verdict(1).is_none());
    }

    #[test]
    fn files_are_owner_only() {
        let (_tmp, dir) = tmp_dir();
        dir.write_request(&request(1)).unwrap();
        let mode = std::fs::metadata(dir.path().join("request-1.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        // No temp file left behind for a reader to trip over.
        assert!(!dir.path().join("request-1.tmp").exists());
    }

    #[test]
    fn a_question_and_its_answer_round_trip() {
        let (_tmp, dir) = tmp_dir();
        let q = Question {
            seq: 1,
            question: "should I migrate the v1 endpoints too?".into(),
            options: vec!["yes".into(), "no".into()],
        };
        dir.write_question(&q).unwrap();
        assert_eq!(dir.read_question(1), Some(q));
        assert_eq!(dir.read_answer(1), None, "unanswered until answered");

        let a = Answer {
            seq: 1,
            answer: "no — v1 is being retired".into(),
        };
        dir.write_answer(&a).unwrap();
        assert_eq!(dir.read_answer(1), Some(a));
    }

    #[test]
    fn an_answer_to_an_earlier_question_never_resolves_a_later_one() {
        let (_tmp, dir) = tmp_dir();
        dir.write_answer(&Answer {
            seq: 1,
            answer: "yes".into(),
        })
        .unwrap();
        assert!(dir.read_answer(2).is_none());
        assert!(dir.read_answer(1).is_some());
    }

    #[test]
    fn questions_and_turn_requests_do_not_collide() {
        // They share a directory and both number from 1, so a question must not be
        // readable as a request (or the parent would adjudicate a budget it was never
        // asked about, and vice versa).
        let (_tmp, dir) = tmp_dir();
        dir.write_request(&request(1)).unwrap();
        dir.write_question(&Question {
            seq: 1,
            question: "which database?".into(),
            options: vec![],
        })
        .unwrap();
        assert_eq!(dir.read_request(1).unwrap().requested, 30);
        assert_eq!(dir.read_question(1).unwrap().question, "which database?");
    }

    #[test]
    fn wrap_up_still_grants_enough_turns_to_write_the_answer() {
        // "Report now" with zero turns to report in would produce the empty result
        // this whole mechanism exists to prevent.
        assert!(Verdict::WrapUp { seq: 1 }.extra_turns() > 0);
        assert_eq!(
            Verdict::Stop {
                seq: 1,
                reason: "no longer needed".into()
            }
            .extra_turns(),
            0
        );
        assert_eq!(
            Verdict::Grant {
                seq: 1,
                iterations: 40
            }
            .extra_turns(),
            40
        );
    }

    #[test]
    fn a_job_id_cannot_escape_the_jobs_directory() {
        // Ids are generated, but they reach this as strings used as path components.
        assert_eq!(sanitize("../../etc"), "______etc");
        assert_eq!(sanitize("1788442206985-sub0"), "1788442206985-sub0");
        assert_eq!(sanitize("a/b"), "a_b");
    }

    #[test]
    fn the_control_channel_never_lives_in_the_workspace() {
        // The reason this module exists. `.cowboy/` is inside the workspace bind and
        // writable from inside the sandbox (the agent's HOME is `{workdir}/.cowboy/
        // home`), so a verdict file there could be written by sandboxed content and
        // would then steer another agent's instructions.
        let Some(root) = jobs_root() else {
            return; // no HOME/XDG_STATE_HOME on this host: nothing to check
        };
        assert!(root.ends_with("cowboy/jobs"), "got {}", root.display());
        let workspace = std::env::current_dir().unwrap();
        assert!(
            !root.starts_with(&workspace),
            "{} must not be inside the workspace {}",
            root.display(),
            workspace.display()
        );
        assert!(
            !root.to_string_lossy().contains(".cowboy"),
            "got {}",
            root.display()
        );
    }

    #[test]
    fn a_write_recreates_a_directory_a_concurrent_cleanup_removed() {
        // `cargo test` runs a binary's tests as threads in one process, so a cleanup in
        // one and a write in another really do race here — the same class of TOCTOU that
        // `ensure_mask_file` was fixed for.
        let (_tmp, dir) = tmp_dir();
        std::fs::remove_dir_all(dir.path()).unwrap();
        dir.write_verdict(&Verdict::WrapUp { seq: 1 })
            .expect("a missing directory must not fail the write");
        assert_eq!(dir.read_verdict(1), Some(Verdict::WrapUp { seq: 1 }));
    }

    #[test]
    fn cleanup_removes_the_directory_and_the_session_dir_it_was_alone_in() {
        let (tmp, dir) = tmp_dir();
        dir.write_request(&request(1)).unwrap();
        dir.cleanup();
        assert!(!dir.path().exists());
        // The per-session parent goes too, so a session that delegated does not leave an
        // empty directory behind forever.
        assert!(!tmp.path().join("jobs/parent").exists());

        // …but a parent with another live job is left alone.
        let a = tmp.path().join("jobs/p2/job-a");
        let b = tmp.path().join("jobs/p2/job-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        ControlDir::at(a).cleanup();
        assert!(b.exists(), "a sibling job must survive");
        assert!(tmp.path().join("jobs/p2").exists());
    }

    #[test]
    fn a_worker_with_no_control_dir_in_its_environment_gets_none() {
        // Supervision is optional: a foreman, or a roster with it disabled, simply has
        // no channel — and that must not be an error.
        if std::env::var(ENV_JOB_CONTROL_DIR).is_err() {
            assert!(ControlDir::from_env().is_none());
        }
    }
}
