//! Background delegation: the **session-scoped** registry of subagent jobs.
//!
//! Delegation used to be a join. `run_subagents` planned every `subagent` call in a
//! turn and awaited the whole batch, so while children ran the foreman made no model
//! calls, ran no other tools, and answered nothing but a cancel — the session looked
//! wedged, and the only way out killed all the work. Here a dispatch instead *records*
//! a job and returns immediately; the loop is fed job news at its iteration boundaries
//! and can keep working, answer the user, or park in `wait`.
//!
//! Two properties this file is responsible for:
//!
//! - **Jobs outlive a turn.** The registry lives on the session, not on the turn, so
//!   interrupting the foreman to correct it does not throw away four running
//!   subagents. That makes an explicit stop (and a guaranteed reap at session end)
//!   necessary rather than optional — see [`JobRegistry::stop_all`].
//! - **Each piece of news lands exactly once.** A result delivered twice would have
//!   the foreman act on it twice; one never delivered would have it wait forever. So
//!   delivery is tracked per job, and per *request sequence* for turn requests.
//!
//! Deliberately free of process spawning: the runner is injected
//! ([`JobRegistry::dispatch`] takes a closure returning a [`JobHandle`]), so the whole
//! state machine is unit-testable without a tokio runtime or a child `cowboy`.

use std::collections::HashMap;
use std::sync::Arc;

use cowboy_core::time::now_ms;
use tokio::sync::Semaphore;

/// A handle to whatever is actually running a job.
///
/// Abstracted so the registry — and its tests — need neither a tokio runtime nor a
/// real child process. The production implementation is a task abort handle, and
/// aborting drops the future holding the child `Command`, whose `kill_on_drop` reaps
/// the process.
pub trait JobHandle: Send + Sync {
    fn abort(&self);
}

impl JobHandle for tokio::task::AbortHandle {
    fn abort(&self) {
        tokio::task::AbortHandle::abort(self)
    }
}

/// A stop switch for background jobs that can be fired from **outside** the agent
/// loop.
///
/// Needed because jobs are session-scoped: the user must be able to say "stop the
/// subagents" while a turn is running, and at that moment the worker cannot touch the
/// registry — the in-flight turn holds `&mut` on the whole loop. A shared token the
/// spawned tasks select on solves that without making the registry itself shared.
///
/// Firing re-arms: the token is replaced, so jobs dispatched afterwards are not born
/// cancelled.
#[derive(Clone, Default)]
pub struct JobStopper(Arc<std::sync::Mutex<tokio_util::sync::CancellationToken>>);

impl JobStopper {
    /// The token in force now, for a spawned job to select on.
    pub fn token(&self) -> tokio_util::sync::CancellationToken {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Stop everything currently running, and re-arm for the next dispatch.
    pub fn stop_all(&self) {
        let mut guard = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old = std::mem::replace(&mut *guard, tokio_util::sync::CancellationToken::new());
        drop(guard);
        old.cancel();
    }
}

/// Where a job is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Dispatched, waiting for a per-provider concurrency permit.
    Pending,
    /// Actually running.
    Running,
    /// Spent its grant (or stalled) and is blocked on the foreman's verdict.
    AwaitingVerdict { seq: u32 },
    /// Blocked on a question for the foreman.
    ///
    /// Distinct from `AwaitingVerdict` because the foreman answers them with different
    /// things — a budget decision versus an answer about the work — and because a job can
    /// be waiting on a question while its budget is untouched.
    AwaitingAnswer { seq: u32 },
    /// Finished, for better or worse.
    Done { ok: bool },
}

impl JobState {
    pub fn is_done(&self) -> bool {
        matches!(self, JobState::Done { .. })
    }

    /// A short word for the UI and the `jobs` tool.
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Pending => "pending",
            JobState::Running => "running",
            JobState::AwaitingVerdict { .. } => "awaiting verdict",
            JobState::AwaitingAnswer { .. } => "asking a question",
            JobState::Done { ok: true } => "done",
            JobState::Done { ok: false } => "failed",
        }
    }
}

/// What a dispatch needs to know, independent of how the child is run.
#[derive(Debug, Clone, Default)]
pub struct JobSpec {
    /// The child's session id, which is also the job id.
    pub id: String,
    /// The `subagent` tool call that asked for this job.
    pub call_id: String,
    /// Display label, e.g. `tests/small`.
    pub label: String,
    /// Resolved model name (for display).
    pub model: String,
    /// The one-line task, for display.
    pub task: String,
    /// Provider key, for the per-provider concurrency throttle.
    pub provider: String,
    /// Turns granted initially, and the ceiling extensions are clamped to. Both 0
    /// when the roster disabled supervision.
    pub granted: u32,
    pub ceiling: u32,
}

/// Something a running job reports back to the registry.
#[derive(Debug, Clone)]
pub enum JobEvent {
    /// Acquired its provider permit and started for real.
    Started { id: String },
    /// Spent its grant and is asking for more turns, with a progress report.
    TurnRequest {
        id: String,
        seq: u32,
        report: String,
        requested: u32,
        used: u32,
    },
    /// Asked the foreman a question about the work and is blocked on the answer.
    Question {
        id: String,
        seq: u32,
        question: String,
        options: Vec<String>,
    },
    /// Exited. `result` is what the foreman gets to read.
    Finished {
        id: String,
        ok: bool,
        result: String,
    },
    /// An informational update that needs no answer — an external harness that has
    /// gone quiet, or one reporting its progress.
    Note { id: String, text: String },
}

impl JobEvent {
    pub fn id(&self) -> &str {
        match self {
            JobEvent::Started { id }
            | JobEvent::TurnRequest { id, .. }
            | JobEvent::Question { id, .. }
            | JobEvent::Finished { id, .. }
            | JobEvent::Note { id, .. } => id,
        }
    }
}

/// One dispatched subagent.
pub struct Job {
    pub id: String,
    pub call_id: String,
    pub label: String,
    pub model: String,
    pub task: String,
    pub state: JobState,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    /// Turns used / granted / ceiling, as last reported by the child.
    pub used: u32,
    pub granted: u32,
    pub ceiling: u32,
    /// The child's final answer, once it has one.
    pub result: Option<String>,
    /// The latest progress report, while a turn request is outstanding.
    pub report: Option<String>,
    /// The outstanding question and its suggested answers, if the job is asking one.
    pub question: Option<(String, Vec<String>)>,
    /// Updates not yet delivered to the foreman (see [`JobEvent::Note`]).
    pub notes: Vec<String>,
    /// How many turns the outstanding request asked for.
    pub requested: u32,
    /// Whether the finished result has been handed to the conversation.
    delivered_result: bool,
    /// Whether the pending → running transition has been announced.
    delivered_start: bool,
    /// The highest request sequence already handed to the conversation, so a second
    /// request from the same job is delivered but the same one is not delivered twice.
    delivered_seq: Option<u32>,
    /// The same, for questions — a separate sequence, so answering a question does not
    /// suppress a turn request that happens to share its number.
    delivered_question: Option<u32>,
    handle: Option<Box<dyn JobHandle>>,
}

impl Job {
    /// Milliseconds elapsed, frozen at completion.
    pub fn elapsed_ms(&self, now: u64) -> u64 {
        self.finished_ms
            .unwrap_or(now)
            .saturating_sub(self.started_ms)
    }
}

/// News the conversation has not seen yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobNews {
    /// A job acquired its concurrency permit and is running for real. UI-only: it says
    /// nothing the model needs, but a client that renders a pane needs the edge (and the
    /// journal needs it to stay self-consistent for replay).
    Started {
        id: String,
        label: String,
        model: String,
    },
    /// A job finished; its result belongs in the foreman's context.
    Finished {
        id: String,
        label: String,
        ok: bool,
        result: String,
    },
    /// An update from a running job, for the foreman's information.
    Note {
        id: String,
        label: String,
        text: String,
    },
    /// A job is blocked on a question about the work.
    Question {
        id: String,
        label: String,
        seq: u32,
        question: String,
        options: Vec<String>,
    },
    /// A job is blocked asking for more turns, and the foreman must adjudicate.
    TurnRequest {
        id: String,
        label: String,
        seq: u32,
        report: String,
        requested: u32,
        used: u32,
        granted: u32,
        ceiling: u32,
    },
}

impl JobNews {
    pub fn id(&self) -> &str {
        match self {
            JobNews::Started { id, .. }
            | JobNews::Finished { id, .. }
            | JobNews::Question { id, .. }
            | JobNews::TurnRequest { id, .. }
            | JobNews::Note { id, .. } => id,
        }
    }
}

/// A snapshot of one job, for the UI and the `jobs` tool.
///
/// The wire type is the only type: a display struct here plus a near-identical one in
/// the protocol is two things to keep in step, and the UI reads exactly this.
pub type JobView = cowboy_core::daemonproto::JobInfo;

/// The session's background jobs.
pub struct JobRegistry {
    /// Insertion order preserved: a foreman reading `jobs` should see them in the
    /// order it dispatched them, not in hash order.
    order: Vec<String>,
    jobs: HashMap<String, Job>,
    /// Per-provider concurrency permits. Session-lived, not per batch: the throttle
    /// exists to keep a burst of same-provider workers from tripping a rate limit, and
    /// a per-batch semaphore stopped doing that as soon as dispatches spanned turns.
    provider_sems: HashMap<String, Arc<Semaphore>>,
    /// Concurrent workers allowed per provider; 0 = unlimited.
    per_provider: usize,
}

impl JobRegistry {
    pub fn new(per_provider: usize) -> Self {
        Self {
            order: Vec::new(),
            jobs: HashMap::new(),
            provider_sems: HashMap::new(),
            per_provider,
        }
    }

    /// Register a job and start it.
    ///
    /// `spawn` is handed the spec and this provider's permit source (`None` when the
    /// throttle is off) and returns a handle that can stop it. Injected so the registry
    /// holds no opinion about how a job runs — and so its tests spawn nothing.
    pub fn dispatch(
        &mut self,
        spec: JobSpec,
        spawn: impl FnOnce(&JobSpec, Option<Arc<Semaphore>>) -> Box<dyn JobHandle>,
    ) {
        let permits = (self.per_provider > 0).then(|| {
            self.provider_sems
                .entry(spec.provider.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(self.per_provider)))
                .clone()
        });
        let handle = spawn(&spec, permits);
        let job = Job {
            id: spec.id.clone(),
            call_id: spec.call_id,
            label: spec.label,
            model: spec.model,
            task: spec.task,
            question: None,
            delivered_question: None,
            notes: Vec::new(),
            // Pending, not Running: with a per-provider cap a dispatched job may wait
            // for a permit, and reporting it as running would misrepresent both the
            // UI and the elapsed time.
            state: JobState::Pending,
            started_ms: now_ms(),
            finished_ms: None,
            used: 0,
            granted: spec.granted,
            ceiling: spec.ceiling,
            result: None,
            report: None,
            requested: 0,
            delivered_result: false,
            delivered_start: false,
            delivered_seq: None,
            handle: Some(handle),
        };
        self.order.push(spec.id.clone());
        self.jobs.insert(spec.id, job);
    }

    /// Fold an event from a running job into its state. Unknown ids are ignored: a
    /// stopped job's last event can arrive after it has been dropped, and that is not
    /// an error worth propagating.
    pub fn apply_event(&mut self, event: JobEvent) {
        let Some(job) = self.jobs.get_mut(event.id()) else {
            return;
        };
        match event {
            JobEvent::Started { .. } => {
                // Only from Pending: a Started arriving after a verdict resumed the job
                // must not un-do the newer state.
                if job.state == JobState::Pending {
                    job.state = JobState::Running;
                }
            }
            JobEvent::TurnRequest {
                seq,
                report,
                requested,
                used,
                ..
            } => {
                job.state = JobState::AwaitingVerdict { seq };
                job.report = Some(report);
                job.requested = requested;
                job.used = used;
            }
            JobEvent::Question {
                seq,
                question,
                options,
                ..
            } => {
                job.state = JobState::AwaitingAnswer { seq };
                job.question = Some((question, options));
            }
            JobEvent::Note { text, .. } => {
                // Queued, not overwritten: two updates between drains are two updates.
                if !matches!(job.state, JobState::Done { .. }) {
                    job.notes.push(text);
                }
            }
            JobEvent::Finished { ok, result, .. } => {
                job.state = JobState::Done { ok };
                job.question = None;
                job.result = Some(result);
                job.report = None;
                job.requested = 0;
                job.finished_ms = Some(now_ms());
                // The task has exited; there is nothing left to abort, and holding the
                // handle would keep it alive for the life of the session.
                job.handle = None;
            }
        }
    }

    /// Record that a question was answered, moving the job back to running.
    pub fn answered(&mut self, id: &str) {
        if let Some(job) = self.jobs.get_mut(id) {
            if matches!(job.state, JobState::AwaitingAnswer { .. }) {
                job.state = JobState::Running;
            }
            job.question = None;
        }
    }

    /// Record that a verdict was sent, moving the job back to running.
    pub fn resume(&mut self, id: &str, extra_turns: u32) {
        if let Some(job) = self.jobs.get_mut(id) {
            if matches!(job.state, JobState::AwaitingVerdict { .. }) {
                job.state = JobState::Running;
            }
            job.granted = (job.granted + extra_turns).min(if job.ceiling == 0 {
                u32::MAX
            } else {
                job.ceiling
            });
            job.report = None;
            job.requested = 0;
        }
    }

    /// Job news the conversation has not seen, oldest job first. Marks each piece
    /// delivered, so a result cannot be acted on twice and a request cannot be
    /// adjudicated twice.
    pub fn drain_undelivered(&mut self) -> Vec<JobNews> {
        let mut news = Vec::new();
        for id in &self.order {
            let Some(job) = self.jobs.get_mut(id) else {
                continue;
            };
            match &job.state {
                JobState::Running
                | JobState::AwaitingVerdict { .. }
                | JobState::AwaitingAnswer { .. }
                | JobState::Done { .. }
                    if !job.delivered_start =>
                {
                    // Announced once, on the first state that means "it got going" — a job
                    // that finishes between two drains must still produce its start edge,
                    // or a client renders a completion for something it never saw begin.
                    job.delivered_start = true;
                    news.push(JobNews::Started {
                        id: job.id.clone(),
                        label: job.label.clone(),
                        model: job.model.clone(),
                    });
                }
                _ => {}
            }
            for text in std::mem::take(&mut job.notes) {
                news.push(JobNews::Note {
                    id: job.id.clone(),
                    label: job.label.clone(),
                    text,
                });
            }
            match &job.state {
                JobState::Done { ok } if !job.delivered_result => {
                    job.delivered_result = true;
                    news.push(JobNews::Finished {
                        id: job.id.clone(),
                        label: job.label.clone(),
                        ok: *ok,
                        result: job.result.clone().unwrap_or_default(),
                    });
                }
                JobState::AwaitingAnswer { seq } if job.delivered_question != Some(*seq) => {
                    job.delivered_question = Some(*seq);
                    let (question, options) = job.question.clone().unwrap_or_default();
                    news.push(JobNews::Question {
                        id: job.id.clone(),
                        label: job.label.clone(),
                        seq: *seq,
                        question,
                        options,
                    });
                }
                JobState::AwaitingVerdict { seq } if job.delivered_seq != Some(*seq) => {
                    job.delivered_seq = Some(*seq);
                    news.push(JobNews::TurnRequest {
                        id: job.id.clone(),
                        label: job.label.clone(),
                        seq: *seq,
                        report: job.report.clone().unwrap_or_default(),
                        requested: job.requested,
                        used: job.used,
                        granted: job.granted,
                        ceiling: job.ceiling,
                    });
                }
                _ => {}
            }
        }
        news
    }

    /// Jobs that have not finished — what makes `final` premature.
    pub fn outstanding(&self) -> Vec<&Job> {
        self.order
            .iter()
            .filter_map(|id| self.jobs.get(id))
            .filter(|j| !j.state.is_done())
            .collect()
    }

    /// True when nothing is running or waiting.
    pub fn is_idle(&self) -> bool {
        self.outstanding().is_empty()
    }

    /// Jobs blocked on a verdict.
    pub fn awaiting_verdict(&self) -> Vec<&Job> {
        self.order
            .iter()
            .filter_map(|id| self.jobs.get(id))
            .filter(|j| matches!(j.state, JobState::AwaitingVerdict { .. }))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&Job> {
        self.jobs.get(id)
    }

    /// Resolve a job by id, tolerating the label or a unique prefix — a model asked to
    /// echo an opaque id back will sometimes paraphrase it, and refusing on a
    /// near-miss wastes a turn on an error message.
    pub fn resolve_id(&self, needle: &str) -> Option<String> {
        let needle = needle.trim();
        if self.jobs.contains_key(needle) {
            return Some(needle.to_string());
        }
        let mut matches = self.order.iter().filter(|id| {
            id.starts_with(needle)
                || self
                    .jobs
                    .get(*id)
                    .is_some_and(|j| j.label == needle && !j.state.is_done())
        });
        let first = matches.next()?;
        matches.next().is_none().then(|| first.clone())
    }

    /// Snapshots for the UI / the `jobs` tool, in dispatch order.
    pub fn views(&self) -> Vec<JobView> {
        let now = now_ms();
        self.order
            .iter()
            .filter_map(|id| self.jobs.get(id))
            .map(|j| JobView {
                id: j.id.clone(),
                label: j.label.clone(),
                model: j.model.clone(),
                task: j.task.clone(),
                state: j.state.as_str().to_string(),
                elapsed_ms: j.elapsed_ms(now),
                used: j.used,
                granted: j.granted,
                ceiling: j.ceiling,
                requested: j.requested,
            })
            .collect()
    }

    /// Stop every unfinished job. Returns the ids stopped.
    ///
    /// Load-bearing, not tidiness: because jobs outlive a turn, nothing else guarantees
    /// a child dies. The worker calls this on session end (and on an explicit "stop
    /// subagents"), and a stopped job is left *undelivered* so the foreman is told what
    /// happened rather than silently missing a result it was waiting for.
    pub fn stop_all(&mut self) -> Vec<String> {
        let ids: Vec<String> = self
            .order
            .iter()
            .filter(|id| self.jobs.get(*id).is_some_and(|j| !j.state.is_done()))
            .cloned()
            .collect();
        for id in &ids {
            self.stop(id);
        }
        ids
    }

    /// Stop one job, if it is still running.
    pub fn stop(&mut self, id: &str) -> bool {
        let Some(job) = self.jobs.get_mut(id) else {
            return false;
        };
        if job.state.is_done() {
            return false;
        }
        if let Some(h) = job.handle.take() {
            h.abort();
        }
        job.state = JobState::Done { ok: false };
        job.finished_ms = Some(now_ms());
        job.report = None;
        job.requested = 0;
        job.result = Some(format!(
            "[stopped] this subagent was stopped before it finished. Whatever it \
             completed is in its session directory ({id}) — resume from that checkpoint \
             rather than redoing the work."
        ));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records aborts instead of running anything.
    #[derive(Default)]
    struct FakeHandle {
        aborts: Arc<AtomicUsize>,
    }
    impl JobHandle for FakeHandle {
        fn abort(&self) {
            self.aborts.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn spec(id: &str, provider: &str) -> JobSpec {
        JobSpec {
            id: id.into(),
            call_id: format!("call-{id}"),
            label: "tests/small".into(),
            model: "cheap".into(),
            task: "run the tests".into(),
            provider: provider.into(),
            granted: 25,
            ceiling: 400,
        }
    }

    /// The news a test cares about: everything except the UI-only start edge.
    fn substantive(news: Vec<JobNews>) -> Vec<JobNews> {
        news.into_iter()
            .filter(|n| !matches!(n, JobNews::Started { .. }))
            .collect()
    }

    /// Dispatch with a no-op handle, capturing the semaphore the registry handed out.
    fn dispatch(reg: &mut JobRegistry, spec: JobSpec) -> Option<Arc<Semaphore>> {
        let mut seen = None;
        reg.dispatch(spec, |_, sem| {
            seen = sem;
            Box::new(FakeHandle::default())
        });
        seen
    }

    #[test]
    fn the_stop_switch_fires_once_and_re_arms() {
        // A job dispatched *after* the user stopped the last batch must not be born
        // cancelled — the switch is a session-lived control, not a one-shot.
        let stopper = JobStopper::default();
        let first = stopper.token();
        stopper.stop_all();
        assert!(first.is_cancelled());
        let second = stopper.token();
        assert!(!second.is_cancelled());
        stopper.stop_all();
        assert!(second.is_cancelled());
    }

    #[test]
    fn a_dispatched_job_starts_pending_and_becomes_running_on_its_permit() {
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "anthropic"));
        assert_eq!(reg.get("j1").unwrap().state, JobState::Pending);
        assert_eq!(reg.outstanding().len(), 1);
        assert!(!reg.is_idle());

        reg.apply_event(JobEvent::Started { id: "j1".into() });
        assert_eq!(reg.get("j1").unwrap().state, JobState::Running);
    }

    #[test]
    fn the_provider_throttle_is_shared_across_dispatch_batches() {
        // The whole point of moving this onto the session: two dispatches in different
        // turns hitting the same provider must contend for the same permits. A
        // per-batch semaphore silently stopped throttling once fan-out spanned turns.
        let mut reg = JobRegistry::new(2);
        let a = dispatch(&mut reg, spec("j1", "anthropic")).expect("a permit source");
        let b = dispatch(&mut reg, spec("j2", "anthropic")).expect("a permit source");
        assert!(Arc::ptr_eq(&a, &b), "same provider → same semaphore");

        let c = dispatch(&mut reg, spec("j3", "openai")).expect("a permit source");
        assert!(!Arc::ptr_eq(&a, &c), "different providers run in parallel");
        assert_eq!(a.available_permits(), 2);
    }

    #[test]
    fn a_zero_cap_means_no_throttle_at_all() {
        let mut reg = JobRegistry::new(0);
        assert!(
            dispatch(&mut reg, spec("j1", "anthropic")).is_none(),
            "no semaphore is handed out when the throttle is off"
        );
    }

    #[test]
    fn the_start_edge_is_announced_once_even_for_a_job_that_finishes_immediately() {
        // A client renders a pane from these edges, so a completion for something it
        // never saw begin is a hole. The edge is emitted on the first state that means
        // "it got going", which covers a job that finishes between two drains.
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        // Pending: nothing to announce yet.
        assert!(reg.drain_undelivered().is_empty());

        reg.apply_event(JobEvent::Started { id: "j1".into() });
        let news = reg.drain_undelivered();
        assert!(matches!(&news[0], JobNews::Started { id, .. } if id == "j1"));
        // Once only.
        assert!(reg.drain_undelivered().is_empty());

        // A second job that goes straight to Done still gets its start edge.
        dispatch(&mut reg, spec("j2", "p"));
        reg.apply_event(JobEvent::Finished {
            id: "j2".into(),
            ok: true,
            result: "fast".into(),
        });
        let news = reg.drain_undelivered();
        assert!(
            news.iter()
                .any(|n| matches!(n, JobNews::Started { id, .. } if id == "j2")),
            "got {news:?}"
        );
        assert!(news
            .iter()
            .any(|n| matches!(n, JobNews::Finished { id, .. } if id == "j2")));
    }

    #[test]
    fn a_finished_result_is_delivered_exactly_once() {
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        // Nothing to say while it runs.
        assert!(reg.drain_undelivered().is_empty());

        reg.apply_event(JobEvent::Finished {
            id: "j1".into(),
            ok: true,
            result: "found the bug in store.rs".into(),
        });
        let news = substantive(reg.drain_undelivered());
        assert_eq!(news.len(), 1);
        match &news[0] {
            JobNews::Finished { id, ok, result, .. } => {
                assert_eq!(id, "j1");
                assert!(ok);
                assert!(result.contains("store.rs"));
            }
            other => panic!("expected a finished result, got {other:?}"),
        }
        // Draining again must not re-deliver it — the foreman would act twice.
        assert!(reg.drain_undelivered().is_empty());
        assert!(reg.is_idle());
    }

    #[test]
    fn a_question_is_delivered_once_and_answering_it_resumes_the_job() {
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        reg.apply_event(JobEvent::Started { id: "j1".into() });
        let _ = reg.drain_undelivered();

        reg.apply_event(JobEvent::Question {
            id: "j1".into(),
            seq: 1,
            question: "migrate the v1 endpoints too?".into(),
            options: vec!["yes".into(), "no".into()],
        });
        assert_eq!(
            reg.get("j1").unwrap().state,
            JobState::AwaitingAnswer { seq: 1 }
        );
        let news = substantive(reg.drain_undelivered());
        assert_eq!(news.len(), 1);
        match &news[0] {
            JobNews::Question {
                question, options, ..
            } => {
                assert!(question.contains("v1 endpoints"));
                assert_eq!(options, &["yes".to_string(), "no".to_string()]);
            }
            other => panic!("expected a question, got {other:?}"),
        }
        // Not twice: the foreman would answer the same question again.
        assert!(reg.drain_undelivered().is_empty());

        reg.answered("j1");
        assert_eq!(reg.get("j1").unwrap().state, JobState::Running);
        assert!(reg.get("j1").unwrap().question.is_none());
        assert!(reg.drain_undelivered().is_empty());
    }

    #[test]
    fn a_question_and_a_turn_request_are_tracked_separately() {
        // They share a job and both number from 1. Delivering one must not suppress the
        // other, or a worker that asked a question and then ran out of turns would be
        // adjudicated for only one of them and hang on the other.
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        reg.apply_event(JobEvent::Started { id: "j1".into() });
        let _ = reg.drain_undelivered();

        reg.apply_event(JobEvent::Question {
            id: "j1".into(),
            seq: 1,
            question: "which database?".into(),
            options: vec![],
        });
        assert_eq!(substantive(reg.drain_undelivered()).len(), 1);
        reg.answered("j1");

        reg.apply_event(JobEvent::TurnRequest {
            id: "j1".into(),
            seq: 1,
            report: "half done".into(),
            requested: 10,
            used: 20,
        });
        let news = substantive(reg.drain_undelivered());
        assert_eq!(news.len(), 1, "the turn request must still be delivered");
        assert!(matches!(&news[0], JobNews::TurnRequest { .. }), "{news:?}");
    }

    #[test]
    fn a_job_that_finishes_while_asking_forgets_the_question() {
        // Otherwise a finished job keeps an outstanding question the foreman could still
        // be told to answer, writing into a control dir that has been cleaned up.
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        reg.apply_event(JobEvent::Question {
            id: "j1".into(),
            seq: 1,
            question: "?".into(),
            options: vec![],
        });
        reg.apply_event(JobEvent::Finished {
            id: "j1".into(),
            ok: true,
            result: "done anyway".into(),
        });
        assert!(reg.get("j1").unwrap().question.is_none());
        assert_eq!(reg.get("j1").unwrap().state, JobState::Done { ok: true });
    }

    #[test]
    fn a_turn_request_is_delivered_once_per_sequence() {
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        reg.apply_event(JobEvent::TurnRequest {
            id: "j1".into(),
            seq: 1,
            report: "mapped the crate; 3 files left".into(),
            requested: 30,
            used: 25,
        });
        assert_eq!(reg.awaiting_verdict().len(), 1);
        let news = substantive(reg.drain_undelivered());
        assert_eq!(news.len(), 1);
        match &news[0] {
            JobNews::TurnRequest {
                seq,
                requested,
                used,
                granted,
                ceiling,
                report,
                ..
            } => {
                assert_eq!((*seq, *requested, *used), (1, 30, 25));
                // The budget columns travel with the request so the foreman can see
                // what an extension costs before granting it.
                assert_eq!((*granted, *ceiling), (25, 400));
                assert!(report.contains("3 files left"));
            }
            other => panic!("expected a turn request, got {other:?}"),
        }
        // Same request again: already adjudicated.
        assert!(reg.drain_undelivered().is_empty());

        // Granted, then it comes back for more: a *new* sequence is new news.
        reg.resume("j1", 30);
        assert_eq!(reg.get("j1").unwrap().state, JobState::Running);
        assert_eq!(reg.get("j1").unwrap().granted, 55);
        reg.apply_event(JobEvent::TurnRequest {
            id: "j1".into(),
            seq: 2,
            report: "one file left".into(),
            requested: 20,
            used: 55,
        });
        assert_eq!(substantive(reg.drain_undelivered()).len(), 1);
    }

    #[test]
    fn a_grant_cannot_push_a_job_past_its_ceiling() {
        // The foreman asks; the host decides. Clamped here as well as in the child's
        // own budget, because neither end should have to trust the other.
        let mut reg = JobRegistry::new(2);
        dispatch(
            &mut reg,
            JobSpec {
                granted: 25,
                ceiling: 40,
                ..spec("j1", "p")
            },
        );
        reg.resume("j1", 100);
        assert_eq!(reg.get("j1").unwrap().granted, 40);
    }

    #[test]
    fn stopping_a_job_aborts_it_and_tells_the_foreman() {
        let aborts = Arc::new(AtomicUsize::new(0));
        let mut reg = JobRegistry::new(2);
        let counter = aborts.clone();
        reg.dispatch(spec("j1", "p"), move |_, _| {
            Box::new(FakeHandle { aborts: counter })
        });

        assert!(reg.stop("j1"));
        assert_eq!(aborts.load(Ordering::Relaxed), 1);
        assert_eq!(reg.get("j1").unwrap().state, JobState::Done { ok: false });
        // Stopping is news: a foreman waiting on this job must learn it is not coming.
        let news = substantive(reg.drain_undelivered());
        assert_eq!(news.len(), 1);
        match &news[0] {
            JobNews::Finished { ok, result, .. } => {
                assert!(!ok);
                assert!(result.contains("[stopped]"), "got: {result}");
                assert!(result.contains("j1"), "names the session to resume from");
            }
            other => panic!("expected a stopped result, got {other:?}"),
        }
        // Stopping twice is a no-op, not a second abort or a second result.
        assert!(!reg.stop("j1"));
        assert_eq!(aborts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stop_all_leaves_nothing_outstanding() {
        let aborts = Arc::new(AtomicUsize::new(0));
        let mut reg = JobRegistry::new(2);
        for id in ["j1", "j2", "j3"] {
            let counter = aborts.clone();
            reg.dispatch(spec(id, "p"), move |_, _| {
                Box::new(FakeHandle { aborts: counter })
            });
        }
        // One already finished: it must not be aborted or re-reported.
        reg.apply_event(JobEvent::Finished {
            id: "j2".into(),
            ok: true,
            result: "done".into(),
        });

        let stopped = reg.stop_all();
        assert_eq!(stopped, vec!["j1".to_string(), "j3".to_string()]);
        assert_eq!(aborts.load(Ordering::Relaxed), 2);
        assert!(reg.is_idle());
        assert!(reg.outstanding().is_empty());
    }

    #[test]
    fn a_late_event_from_a_stopped_job_is_ignored() {
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        reg.stop("j1");
        // The child got one more line out before it died.
        reg.apply_event(JobEvent::Finished {
            id: "j1".into(),
            ok: true,
            result: "actually I finished!".into(),
        });
        // It is allowed to overwrite the result — it did finish — but an event for a
        // job that no longer exists must not panic.
        reg.apply_event(JobEvent::Started {
            id: "does-not-exist".into(),
        });
        assert!(reg.is_idle());
    }

    #[test]
    fn a_started_event_cannot_undo_a_newer_state() {
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        reg.apply_event(JobEvent::Finished {
            id: "j1".into(),
            ok: true,
            result: "done".into(),
        });
        reg.apply_event(JobEvent::Started { id: "j1".into() });
        assert_eq!(reg.get("j1").unwrap().state, JobState::Done { ok: true });
    }

    #[test]
    fn views_are_in_dispatch_order_and_carry_the_budget_columns() {
        let mut reg = JobRegistry::new(2);
        for id in ["j1", "j2"] {
            dispatch(&mut reg, spec(id, "p"));
        }
        reg.apply_event(JobEvent::TurnRequest {
            id: "j2".into(),
            seq: 1,
            report: "r".into(),
            requested: 30,
            used: 25,
        });
        let views = reg.views();
        assert_eq!(
            views.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            vec!["j1", "j2"]
        );
        assert_eq!(views[0].state, "pending");
        assert_eq!(views[1].state, "awaiting verdict");
        assert_eq!(views[1].requested, 30);
        assert_eq!(
            (views[1].used, views[1].granted, views[1].ceiling),
            (25, 25, 400)
        );
    }

    #[test]
    fn a_job_can_be_named_by_id_prefix_or_label() {
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("1788442206985-sub0", "p"));
        assert_eq!(
            reg.resolve_id("1788442206985-sub0").as_deref(),
            Some("1788442206985-sub0")
        );
        // A prefix resolves …
        assert_eq!(
            reg.resolve_id("1788442206985").as_deref(),
            Some("1788442206985-sub0")
        );
        // … and so does the label of a live job, which is what a model tends to echo.
        assert_eq!(
            reg.resolve_id("tests/small").as_deref(),
            Some("1788442206985-sub0")
        );
        assert!(reg.resolve_id("nope").is_none());

        // An ambiguous prefix resolves to nothing rather than to the wrong job.
        dispatch(&mut reg, spec("1788442206985-sub1", "p"));
        assert!(reg.resolve_id("1788442206985").is_none());
    }

    #[test]
    fn elapsed_time_freezes_when_a_job_finishes() {
        let mut reg = JobRegistry::new(2);
        dispatch(&mut reg, spec("j1", "p"));
        reg.apply_event(JobEvent::Finished {
            id: "j1".into(),
            ok: true,
            result: "done".into(),
        });
        let job = reg.get("j1").unwrap();
        let far_future = now_ms() + 60_000;
        assert!(
            job.elapsed_ms(far_future) < 60_000,
            "a finished job's clock must stop"
        );
    }
}
