//! Pure helpers for the agent loop: no `AgentLoop` state, just data in → data
//! out (rendering, parsing, diffing, truncation). Split out of `run/mod.rs` to
//! keep the loop itself focused on orchestration.

use super::*;
use std::path::PathBuf;

/// Path to *this* cowboy binary, for spawning subagents.
///
/// Delegates to [`crate::project::self_exe`], which is robust to the binary being
/// replaced mid-session. Shared with the sandbox's shim bind, which hits the same
/// problem far less visibly — see that function.
pub(super) fn self_exe() -> std::result::Result<PathBuf, String> {
    crate::project::self_exe()
}

/// Whether the process with this pid is gone.
///
/// One direction of this is reliable and that is the direction we use: a pid that
/// cannot be signalled at all means the process is definitely gone. The converse is not certain (a
/// recycled pid could be a different process), and the cost of that is a worker running
/// slightly longer than it needed to — much cheaper than killing a live worker because
/// the check guessed wrong.
pub(super) fn process_is_gone(pid: u32) -> bool {
    !crate::project::pid_alive(pid)
}

/// The effective delegation depth limit for a roster: the configured `max_depth`
/// when a worker may not itself delegate, else the hard ceiling — always clamped
/// by [`MAX_SUBAGENT_DEPTH`].
pub(super) fn effective_max_depth(crew_cfg: Option<&cowboy_core::crew::CrewConfig>) -> usize {
    match crew_cfg {
        Some(c) if !c.delegation.allow_recursive_delegation => c.delegation.max_depth as usize,
        _ => MAX_SUBAGENT_DEPTH,
    }
    .min(MAX_SUBAGENT_DEPTH)
}

/// Whether a loop may delegate at all: crew mode is on **and** it is not already
/// at the depth limit.
///
/// One predicate, used by three call sites that previously disagreed. The tool
/// surface and the foreman guidance were gated on crew mode alone, while
/// `plan_subagent` refused the call at depth — so a max-depth child was told to
/// delegate, tried, and spent a model round-trip learning it could not ("Delegation
/// isn't available at this depth"). The runtime check in `plan_subagent` stays as
/// the authority; this only stops us advertising what we will refuse.
pub(super) fn delegation_available(
    crew_on: bool,
    depth: usize,
    crew_cfg: Option<&cowboy_core::crew::CrewConfig>,
) -> bool {
    crew_on && depth < effective_max_depth(crew_cfg)
}

/// The tool surface for a loop: everything, minus the delegation tools it cannot use.
///
/// Two independent gates, because the two roles are different. A loop that can
/// delegate gets `subagent` and the tools for supervising what it dispatched
/// (`jobs`/`wait`/`job_reply`). A loop that *is* a supervised worker gets
/// `request_turns` — and a foreman must not, because it has no foreman to ask.
pub(super) fn tool_surface(delegation_available: bool, can_request_turns: bool) -> Vec<ToolDef> {
    let foreman_only = [
        tools::TOOL_SUBAGENT,
        tools::TOOL_JOBS,
        tools::TOOL_WAIT,
        tools::TOOL_JOB_REPLY,
    ];
    tools::definitions()
        .into_iter()
        .filter(|t| delegation_available || !foreman_only.contains(&t.name.as_str()))
        .filter(|t| can_request_turns || t.name != tools::TOOL_REQUEST_TURNS)
        .collect()
}

/// Tool calls that are pure coordination: checking on jobs, waiting for one, answering
/// a worker's request for turns.
///
/// The loop guards must skip a batch of these. A foreman with four subagents in flight
/// legitimately calls `jobs` — or `wait` — several times with identical arguments and
/// gets identical output, which is exactly the shape the repetition guard aborts a turn
/// for. They are also not *investigation*, so they must not feed the novelty metric
/// either: waiting for a worker is not going in circles.
pub(super) fn is_coordination_only(calls: &[cowboy_core::model::ToolCall]) -> bool {
    !calls.is_empty()
        && calls.iter().all(|c| {
            matches!(
                c.name.as_str(),
                tools::TOOL_JOBS | tools::TOOL_WAIT | tools::TOOL_JOB_REPLY
            )
        })
}

/// The system prompt for a loop: the base, plus foreman guidance when it can
/// delegate, plus subagent guidance when it *is* one.
pub(super) fn system_prompt(
    delegation_available: bool,
    subagent_depth: usize,
    can_request_turns: bool,
    roster: Option<&cowboy_core::crew::CrewConfig>,
) -> String {
    let mut system = String::from(SYSTEM_PROMPT);
    if delegation_available {
        // Built from the roster so the categories the foreman is told about are
        // exactly the ones that can actually route.
        system.push_str(&foreman_prompt(roster));
    }
    // A worker spawned as a subagent gets extra guidance to stream large outputs
    // to a file rather than risk losing them to a truncated tool call.
    if subagent_depth > 0 {
        system.push_str(SUBAGENT_PROMPT);
    }
    // Only mentioned when the channel to ask actually exists; otherwise it is advice
    // the worker cannot act on.
    if can_request_turns {
        system.push_str(TURN_REQUEST_PROMPT);
    }
    system
}

/// The prompt with the sandbox's real paths in place of the Linux defaults it is
/// written with (`/workspace`, `/tmp`).
///
/// A prompt that names a directory the agent cannot write sends it straight into a
/// refusal: on macOS the project is at its host path and `/tmp` is the host's own,
/// which the sandbox cannot touch.
pub(super) fn with_sandbox_paths(prompt: String, paths: &crate::sandbox::SandboxPaths) -> String {
    if *paths == crate::sandbox::SandboxPaths::default() {
        return prompt;
    }
    replace_path_token(
        &replace_path_token(&prompt, "/workspace", &paths.workdir),
        "/tmp",
        &paths.scratch,
    )
}

/// Replace `token` where it stands as a whole path, not as a prefix of a longer word.
fn replace_path_token(s: &str, token: &str, with: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(token) {
        let after = &rest[i + token.len()..];
        let whole = after
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-'));
        out.push_str(&rest[..i]);
        out.push_str(if whole { with } else { token });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// A worker's iteration budget for one turn: how many turns it has been granted,
/// the host-enforced total it can never be granted past, and what it has spent.
///
/// The foreman is *unsupervised* — it holds `agent.max_iterations` with nobody to
/// ask for more, so `ceiling == grant` and the request machinery stays dormant. A
/// delegated worker is supervised: it starts with a small effort-scaled grant and
/// must report progress to earn extensions, up to `ceiling`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IterationBudget {
    /// Turns granted so far (initial grant + extensions).
    pub granted: u32,
    /// Hard ceiling on `granted`, whatever the foreman says.
    pub ceiling: u32,
    /// Turns consumed this turn.
    pub used: u32,
    /// Whether this worker has someone to ask for more turns.
    pub supervised: bool,
}

impl IterationBudget {
    /// Read a delegated worker's budget from the environment the parent set, falling
    /// back to `max_iterations` for a foreman (or any worker whose roster disabled
    /// supervision by setting `max_total_iterations: 0`).
    pub fn from_env(max_iterations: u32) -> Self {
        let read =
            |k: &str| -> Option<u32> { std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok()) };
        Self::resolve(
            read(ENV_ITERATION_GRANT),
            read(ENV_MAX_TOTAL_ITERATIONS),
            max_iterations,
        )
    }

    /// The env-independent half of [`Self::from_env`], so the fallback rules are
    /// testable without mutating process environment (which races under `cargo test`,
    /// where a binary's tests share one process).
    ///
    /// A malformed, zero, or half-present pair falls back rather than being trusted: a
    /// worker that believes it has 0 turns cannot make its first model call, which
    /// would turn a config typo into "delegation silently does nothing".
    pub fn resolve(grant: Option<u32>, ceiling: Option<u32>, max_iterations: u32) -> Self {
        match (grant.filter(|n| *n > 0), ceiling.filter(|n| *n > 0)) {
            (Some(grant), Some(ceiling)) => Self {
                granted: grant.min(ceiling),
                ceiling,
                used: 0,
                supervised: true,
            },
            _ => Self {
                granted: max_iterations,
                ceiling: max_iterations,
                used: 0,
                supervised: false,
            },
        }
    }

    /// Turns left before the current grant runs out.
    pub fn remaining(&self) -> u32 {
        self.granted.saturating_sub(self.used)
    }
    /// Whether the grant is spent.
    pub fn exhausted(&self) -> bool {
        self.used >= self.granted
    }

    /// Extend by `n` turns, clamped to the ceiling. Returns how many were actually
    /// added — `0` means the ceiling is reached and no further grant is possible,
    /// which the caller must report rather than looping on a request that can never
    /// be satisfied. The clamp is host-side on purpose: the foreman asks, the host
    /// decides, so a confused (or captured) foreman cannot grant its way past the
    /// bound.
    pub fn extend(&mut self, n: u32) -> u32 {
        let headroom = self.ceiling.saturating_sub(self.granted);
        let added = n.min(headroom);
        self.granted += added;
        added
    }

    /// Extend past the ceiling, on explicit human consent.
    ///
    /// The ceiling exists to bound an agent nobody is watching — it is a stand-in for
    /// human judgement about "is this still worth running?". When a person has actually
    /// been asked and said yes, that judgement is present, so the ceiling moves with the
    /// grant rather than silently refusing.
    ///
    /// Deliberately a separate method from [`Self::extend`], which clamps: no automated
    /// path — a foreman granting a worker more turns, an unanswered-request auto-extension
    /// — can reach this by accident. The only caller is the one that has an answer from a
    /// human in hand, and the number of times it may be called is capped by its own
    /// counter.
    pub fn extend_with_consent(&mut self, n: u32) -> u32 {
        self.granted += n;
        self.ceiling = self.ceiling.max(self.granted);
        n
    }
}

/// How close a worker is to spending its grant. Ordered, so the loop can fire each
/// nudge exactly once by remembering the highest stage it has announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum GrantStage {
    /// Plenty left; say nothing.
    Fine,
    /// ~70% spent: time to think about converging or asking for more.
    Nudge,
    /// ~90% spent: converge or ask now.
    Urgent,
}

/// The nudge stage for `used` of `granted` turns.
///
/// Proportional rather than "N turns left" so it works for a 15-turn tiny grant and
/// a 400-turn ceiling alike. A worker that is never told it is running out spends
/// its last turn mid-exploration and returns a `[partial]` — the observed failure.
pub(super) fn grant_stage(used: u32, granted: u32) -> GrantStage {
    if granted == 0 {
        return GrantStage::Urgent;
    }
    // Integer arithmetic, no floats: used/granted >= 9/10, then >= 7/10.
    if used * 10 >= granted * 9 {
        GrantStage::Urgent
    } else if used * 10 >= granted * 7 {
        GrantStage::Nudge
    } else {
        GrantStage::Fine
    }
}

/// The nudge text for a stage. A supervised worker is pointed at `request_turns`
/// (it has a foreman to ask); an unsupervised one is only told to converge, since
/// there is nobody on the other end.
pub(super) fn grant_notice(stage: GrantStage, b: &IterationBudget) -> Option<String> {
    let (used, granted, left) = (b.used, b.granted, b.remaining());
    match (stage, b.supervised) {
        (GrantStage::Fine, _) => None,
        (GrantStage::Nudge, true) => Some(format!(
            "iteration budget: {used}/{granted} turns used ({left} left). If this task \
             genuinely needs more, call `request_turns` with a progress report; otherwise \
             start converging."
        )),
        (GrantStage::Nudge, false) => Some(format!(
            "iteration budget: {used}/{granted} turns used ({left} left) — start converging \
             on an answer."
        )),
        (GrantStage::Urgent, true) => Some(format!(
            "iteration budget nearly spent ({used}/{granted}, {left} left). Either call \
             `request_turns` now with a progress report and how many more turns you need, or \
             write up what you have and call `final`. Do not run out mid-exploration."
        )),
        (GrantStage::Urgent, false) => Some(format!(
            "iteration budget nearly spent ({used}/{granted}, {left} left) — write up what you \
             have and call `final` now."
        )),
    }
}

/// Host-recorded evidence that the project's own checks were run and passed since
/// the last edit.
///
/// The point is that this is *measured*, not self-reported. "I ran the tests and
/// they pass" in a `final` message is the model's account of itself; this is a
/// record of which commands actually exited 0, kept by the loop that ran them. The
/// same distinction `build_partial_result` already draws for artifacts.
///
/// Off unless the project nominated commands (`agent.verify`), because a gate
/// invented by the harness would refuse completion over a check the repo never
/// asked for.
#[derive(Debug, Default)]
pub(super) struct Verification {
    /// The commands that must pass, as resolved from config. Empty = gate off.
    required: Vec<String>,
    /// Required commands observed exiting 0 since the last edit.
    passed: std::collections::HashSet<String>,
    /// Whether any file has been changed since the last full pass.
    dirty: bool,
}

impl Verification {
    pub(super) fn new(required: Vec<String>) -> Self {
        Self {
            required,
            ..Default::default()
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        !self.required.is_empty()
    }

    /// Record that the workspace changed, which invalidates every earlier pass: a
    /// test run only vouches for the tree it ran against.
    pub(super) fn note_edit(&mut self) {
        self.dirty = true;
        self.passed.clear();
    }

    /// Record a finished command. Matching is on the normalized command text, so
    /// the same check counts whether or not the model added whitespace; a non-zero
    /// exit deliberately does *not* count, and clears any earlier pass of that same
    /// command so a passing run followed by a failing one is not treated as verified.
    pub(super) fn note_command(&mut self, command: &str, exit_code: i32) {
        let Some(req) = self.match_required(command) else {
            return;
        };
        if exit_code == 0 {
            self.passed.insert(req);
        } else {
            self.passed.remove(&req);
        }
    }

    /// The required command `command` counts as, if any.
    ///
    /// A command counts when the required text appears within it, so the usual
    /// wrappers still register: `cd crates/x && cargo test`, or `cargo test 2>&1 |
    /// tail`. Narrower than matching per-word (which would let `cargo build` satisfy
    /// a `cargo test` requirement) and looser than equality (which nothing real
    /// would ever satisfy).
    fn match_required(&self, command: &str) -> Option<String> {
        let norm = squeeze_cmd(command);
        self.required
            .iter()
            .find(|r| norm.contains(&squeeze_cmd(r)))
            .cloned()
    }

    /// Required commands with no passing run against the current tree.
    pub(super) fn outstanding(&self) -> Vec<&str> {
        self.required
            .iter()
            .filter(|r| !self.passed.contains(*r))
            .map(|r| r.as_str())
            .collect()
    }

    /// Whether finishing now would leave edits unchecked — the condition the
    /// `final` gate acts on. Clean sessions (a question, a review, an
    /// investigation) are never gated, since there is nothing to verify.
    pub(super) fn has_unverified_edits(&self) -> bool {
        self.is_enabled() && self.dirty && !self.outstanding().is_empty()
    }
}

/// Collapse whitespace so command comparison ignores formatting.
fn squeeze_cmd(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a worker has already seen, so the loop can tell "still working" from "going
/// in circles" **without asking the model**.
///
/// This is deliberately narrower than the existing signature guards, and they cover
/// different failures. `tool_repeat`/`same_call_repeat` compare one iteration's tool
/// *batch* with the previous one, so they only fire on repetition; a worker that
/// wanders across many different files never trips them. This tracker instead asks
/// "did this iteration learn anything new?" — a new file, an edit, a new command, or
/// changed output from an old one. A tight loop goes barren immediately; broad
/// wandering does not, and is caught by the iteration grant instead. Two mechanisms,
/// two failure modes.
#[derive(Debug, Default)]
pub(super) struct ProgressTracker {
    /// Read key → the step it was first read at (for the "already read" pointer).
    reads: std::collections::HashMap<String, u32>,
    /// Content digest per read key, so an unchanged re-read can be elided while a
    /// changed one passes through.
    read_digests: std::collections::HashMap<String, u64>,
    /// Normalized signatures of commands already run.
    commands: std::collections::HashSet<String>,
    /// Consecutive iterations that produced nothing new.
    barren: u32,
    /// Read outcomes recorded since the last `observe`, drained by it. Reads are
    /// classified where the *content* comes back (`note_read`) rather than from the
    /// call's arguments, because "did this teach us anything" is a fact about the
    /// bytes, not about the request. One owner, so a first read cannot be
    /// miscounted as a re-read by a second bookkeeper.
    pending_new_reads: u32,
    pending_rereads: u32,
    /// Totals since the last progress report, for the evidence block.
    files_read: u32,
    files_edited: u32,
    commands_run: u32,
    reread_count: u32,
}

/// What one iteration's tool calls actually accomplished.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Novelty {
    pub new_reads: u32,
    pub edits: u32,
    pub new_commands: u32,
    /// A re-read of a file already in context: the cheapest way to look busy.
    pub rereads: u32,
    /// An old command whose output changed — legitimate polling, so not barren.
    pub changed_output: bool,
}

impl Novelty {
    /// Nothing new happened this iteration.
    pub fn is_barren(&self) -> bool {
        self.new_reads == 0 && self.edits == 0 && self.new_commands == 0 && !self.changed_output
    }
}

impl ProgressTracker {
    /// The cache key for a read: the path plus any window, so a full read and a
    /// windowed read of the same file are distinct observations.
    pub fn read_key(path: &str, offset: Option<usize>, limit: Option<usize>) -> String {
        match (offset, limit) {
            (None, None) => path.to_string(),
            (o, l) => format!("{path}@{}:{}", o.unwrap_or(0), l.unwrap_or(0)),
        }
    }

    /// Record a completed read. Returns `Some(step)` when this exact read was already
    /// made and the content has not changed since — the caller elides the body and
    /// points at the earlier one instead.
    pub fn note_read(&mut self, key: &str, content: &str, step: u32) -> Option<u32> {
        let digest = digest(content);
        let prior_step = self.reads.get(key).copied();
        let unchanged = self.read_digests.get(key) == Some(&digest);
        self.read_digests.insert(key.to_string(), digest);
        match prior_step {
            Some(step) if unchanged => {
                self.reread_count += 1;
                self.pending_rereads += 1;
                Some(step)
            }
            // Read before, but it changed (the agent edited it, a build wrote it):
            // the new bytes are new information and must reach the model.
            Some(_) => {
                self.pending_new_reads += 1;
                None
            }
            None => {
                self.reads.insert(key.to_string(), step);
                self.files_read += 1;
                self.pending_new_reads += 1;
                None
            }
        }
    }

    /// Classify an iteration's tool calls, updating the barren streak. Consumes the
    /// read outcomes recorded by [`Self::note_read`] during this iteration, so it must
    /// be called once per iteration *after* the tools have run.
    ///
    /// `output_changed` is the loop's existing "same call, different result" signal,
    /// which keeps genuine polling (`sleep 5 && curl health`) off the barren path.
    pub fn observe(
        &mut self,
        calls: &[cowboy_core::model::ToolCall],
        output_changed: bool,
    ) -> Novelty {
        let mut n = Novelty {
            changed_output: output_changed,
            new_reads: std::mem::take(&mut self.pending_new_reads),
            rereads: std::mem::take(&mut self.pending_rereads),
            ..Default::default()
        };
        for c in calls {
            match c.name.as_str() {
                tools::TOOL_EDIT | tools::TOOL_WRITE => {
                    n.edits += 1;
                    self.files_edited += 1;
                }
                tools::TOOL_SHELL => {
                    let sig = normalize_shell_args(&c.arguments);
                    self.commands_run += 1;
                    if self.commands.insert(sig) {
                        n.new_commands += 1;
                    }
                }
                // Reads are accounted in `note_read`. Anything else (plan, memory,
                // artifact, delegation, a turn request) is bookkeeping, not
                // investigation: neither progress nor circling.
                _ => {}
            }
        }
        if n.is_barren() {
            self.barren += 1;
        } else {
            self.barren = 0;
        }
        n
    }

    /// Consecutive barren iterations.
    pub fn barren_streak(&self) -> u32 {
        self.barren
    }

    /// Whether the worker has been going in circles for `window` iterations.
    /// `window == 0` disables the check.
    pub fn stalled(&self, window: u32) -> bool {
        window > 0 && self.barren >= window
    }

    /// Objective evidence for a progress report: what the worker has actually
    /// touched. Attached host-side so the foreman adjudicates a turn request against
    /// measurements, not against the worker's own account of itself.
    pub fn evidence(&self) -> String {
        format!(
            "files read: {} · files edited: {} · commands run: {} · unchanged re-reads: {} · \
             consecutive iterations with nothing new: {}",
            self.files_read, self.files_edited, self.commands_run, self.reread_count, self.barren
        )
    }

    /// Clear the barren streak — e.g. after a redirect, so the worker isn't
    /// immediately reported as stalled for the loop it was just pulled out of.
    pub fn clear_streak(&mut self) {
        self.barren = 0;
    }
}

/// The observation that replaces an unchanged re-read. Names the earlier step so the
/// model can find the content it already has, and says what to do instead — a bare
/// refusal just invites a retry.
pub(super) fn reread_notice(path: &str, prior_step: u32) -> String {
    format!(
        "(not re-read) `{path}` is unchanged since you read it at step {prior_step}; its \
         contents are already earlier in this conversation. Scroll back rather than \
         re-reading. If you need a different part of the file, read it with an explicit \
         offset/limit; if you are looking for something specific, grep for it; otherwise \
         move on to the next step."
    )
}

/// A stable 64-bit digest, for "is this the same content as last time".
fn digest(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Forward a streamed [`Delta`] to the UI. A free function so it borrows only
/// the UI, not all of `self` (the in-flight chat future holds an immutable
/// borrow of the loop).
pub(super) fn emit_delta(ui: &mut dyn AgentUi, piece: Delta) {
    match piece {
        Delta::Content(t) => ui.model_delta(&t),
        Delta::Reasoning(t) => ui.model_reasoning(&t),
    }
}

/// Render a span of messages as plain text for the compaction summarizer.
pub(super) fn render_transcript(messages: &[Message]) -> String {
    let mut s = String::new();
    for m in messages {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        s.push_str(&format!("[{role}]\n"));
        if !m.content.is_empty() {
            s.push_str(&m.content);
            s.push('\n');
        }
        for tc in &m.tool_calls {
            s.push_str(&format!("(tool call {}: {})\n", tc.name, tc.arguments));
        }
        s.push('\n');
    }
    s
}

pub(super) fn parse_args<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T> {
    let args = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str(args).map_err(|e| anyhow::anyhow!("invalid tool arguments: {e}"))
}

/// Render a [`HandoffArgs`] into the canonical `handoff.md` markdown.
pub(super) fn render_handoff_md(a: &HandoffArgs) -> String {
    let mut s = String::from("# Handoff\n\n");
    s.push_str(&format!("## Goal\n{}\n\n", a.goal.trim()));
    s.push_str(&format!("## Status\n{}\n", a.status.trim()));
    let section = |title: &str, body: &Option<String>| -> String {
        match body {
            Some(b) if !b.trim().is_empty() => format!("\n## {title}\n{}\n", b.trim()),
            _ => String::new(),
        }
    };
    s.push_str(&section("Changed files", &a.changed_files));
    s.push_str(&section("Decisions", &a.decisions));
    s.push_str(&section("Contracts / interfaces", &a.contracts));
    s.push_str(&section("Validation", &a.validation));
    s.push_str(&section("Risks", &a.risks));
    s.push_str(&section("Next steps", &a.next_steps));
    s
}

/// Render a plan as check-boxed lines (for the model observation / console).
pub(super) fn render_plan(plan: &[(String, String)]) -> String {
    plan.iter()
        .map(|(step, status)| {
            let mark = match status.as_str() {
                "done" => "[x]",
                "in_progress" => "[~]",
                _ => "[ ]",
            };
            format!("{mark} {step}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A concise one-line summary of a file op for the UI: the helper's status line
/// on success, or `"<action> <path> — failed"` otherwise.
pub(super) fn fileop_summary(action: &str, path: &str, exit: i32, output: &str) -> String {
    if exit == 0 {
        let line = output.trim();
        if line.is_empty() {
            format!("{action} {path}")
        } else {
            line.to_string()
        }
    } else {
        format!("{action} {path} — failed")
    }
}

/// Build a unified diff (`--- a/path` / `+++ b/path` headers + hunks) of a file
/// change, capped at `max_lines` rendered lines (a trailing marker notes the
/// elision). Returns empty for an unchanged or binary-looking file.
pub(super) fn unified_diff(path: &str, before: &str, after: &str, max_lines: usize) -> String {
    // Skip likely-binary content (NUL bytes) — a diff would be noise.
    if before.contains('\u{0}') || after.contains('\u{0}') {
        return String::new();
    }
    let diff = similar::TextDiff::from_lines(before, after);
    let body = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    if body.trim().is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() > max_lines {
        let kept = lines[..max_lines].join("\n");
        let hidden = lines.len() - max_lines;
        format!("{kept}\n… {hidden} more diff lines (see the file)")
    } else {
        body
    }
}

/// How a file-op's output is cut down to fit the observation budget.
///
/// Not a detail: each of the three tools puts the part the agent cannot do without
/// in a different place, and one blanket rule loses it for two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trim {
    /// Keep the head. `edit`/`write` produce a line or two, so this never bites.
    Head,
    /// Keep both ends. `grep`/`ls` deliberately put their true totals **last**, so
    /// head-only truncation drops exactly the summary that tells the agent how much
    /// it did not see — and leaves it believing it saw everything.
    Ends,
    /// Keep the head, then say where the cut landed. A `read` window is ordered, so
    /// the front is what matters; but its "… N more lines, continue with offset=X"
    /// hint is at the end and goes over the cliff with everything else, leaving a
    /// window that stops mid-file with no sign that it did.
    Read,
}

/// What the agent has seen of a file, so a full overwrite can be checked against it.
///
/// Keyed by the path string the agent used, and stores a digest rather than the
/// content: the point is only to answer "are the bytes on disk still the bytes this
/// session last observed?", and holding every file read in a long session in memory
/// to answer it would be wasteful.
#[derive(Debug, Default)]
pub(super) struct SeenFiles {
    digests: std::collections::HashMap<String, u64>,
}

/// How the current content of a file relates to what the agent last saw of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeenStatus {
    /// The file is exactly as the agent last observed it.
    Match,
    /// The agent has never observed this file in this session.
    Unseen,
    /// Someone else has written to it since.
    Changed,
}

impl SeenFiles {
    /// Record the current content of `path` as observed. `None` (an unreadable or
    /// non-UTF-8 file) forgets it rather than recording a digest of nothing.
    pub(super) fn note(&mut self, path: &str, content: &Option<String>) {
        match content {
            Some(c) => {
                self.digests.insert(path.to_string(), digest(c));
            }
            None => {
                self.digests.remove(path);
            }
        }
    }

    pub(super) fn status(&self, path: &str, current: &str) -> SeenStatus {
        match self.digests.get(path) {
            None => SeenStatus::Unseen,
            Some(d) if *d == digest(current) => SeenStatus::Match,
            Some(_) => SeenStatus::Changed,
        }
    }
}

/// Lines of the applied diff handed back to the *model* after an edit or write.
///
/// Much tighter than the UI's cap: this is paid for in context on every edit, and
/// its job is to confirm placement, not to reproduce the file.
const MODEL_DIFF_LINES: usize = 32;

/// Report an applied change back to the model as a diff, so it can see what landed
/// without re-reading the file.
///
/// `edit` used to answer "edited x.rs: 1 edit applied, 1 replacement" — true, and
/// yet it says nothing about *where* the text went or what now surrounds it. A model
/// that wants to be sure re-reads the file (a whole round trip, for a change it just
/// made), and one that does not carries on against an assumed result. A few lines of
/// context are much cheaper than either.
pub(super) fn applied_change_note(path: &str, before: &str, after: &str) -> Option<String> {
    let diff = unified_diff(path, before, after, MODEL_DIFF_LINES);
    if diff.trim().is_empty() {
        return None;
    }
    // Drop the `--- a/x`/`+++ b/x` header: the path is already in the result line
    // above, and two lines of every edit's budget is worth reclaiming.
    let body: Vec<&str> = diff
        .lines()
        .skip_while(|l| l.starts_with("---") || l.starts_with("+++"))
        .collect();
    if body.is_empty() {
        return None;
    }
    Some(format!("applied change:\n{}\n", body.join("\n")))
}

/// Tell a `read` whose output was cut by the byte cap where it actually got to.
///
/// `read` puts its own continuation hint ("… N more lines; read with offset=X") at
/// the end, which is exactly what head truncation throws away — leaving the model
/// with a window that stops mid-file and no sign that it did. The last surviving
/// gutter line says where the cut landed, which is all the hint needs.
pub(super) fn read_continuation_hint(truncated: &str) -> Option<String> {
    let last = truncated.lines().rev().find_map(|l| {
        l.split_once('\t')
            .and_then(|(n, _)| n.trim().parse::<u64>().ok())
    })?;
    Some(format!(
        "\n[the output cap cut this read at line {last}; continue with offset={}]",
        last + 1
    ))
}

/// A wall-clock duration, for the `[exit code: …]` line.
///
/// The model has no clock: without this it cannot tell a 0.2s unit test from a
/// 9-minute one, which is exactly what it needs to know before choosing a
/// `timeout_seconds`, deciding whether to re-run the whole suite, or judging whether
/// a command it just changed got faster.
pub(super) fn fmt_duration(ms: u128) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// What to tell the model about a command that did not simply finish.
///
/// A bare `[exit code: 124]` is a puzzle: 124 is indistinguishable from a real
/// failing exit status, and the two useful reactions — raise the timeout, or stop
/// running a server in the foreground — are not deducible from it. Naming the
/// timeout that fired and both options turns a repeat of the same ten-minute hang
/// into one decision.
pub(super) fn shell_outcome_note(exit: i32, timeout_secs: u64, ceiling: u64) -> String {
    match exit {
        crate::sandbox::EXIT_TIMEOUT => format!(
            "\n[timed out: the command was killed after {timeout_secs}s and its exit status \
             is unknown. If it genuinely needs longer, re-run it with a larger \
             `timeout_seconds` (up to {ceiling}). If it is a server, watcher or REPL that \
             never exits on its own, do not re-run it in the foreground — start it with \
             the `proc` tool and then test against it.]"
        ),
        crate::sandbox::EXIT_CANCELLED => {
            "\n[cancelled: the user interrupted this command. Do not simply re-run it — \
             read what they asked for next.]"
                .to_string()
        }
        _ => String::new(),
    }
}

/// A stable signature for a turn's tool calls (name + arguments), order-
/// independent so parallel calls in a different order still compare equal. Used
/// by the loop guard to detect an agent re-issuing the identical action.
///
/// `shell` arguments are normalized ([`normalize_shell_args`]) so a model that
/// re-runs the *same inspection* with cosmetic churn — appending `| wc -l`, a
/// trailing `; echo "..."` probe, a `2>&1`, or just fiddling whitespace — still
/// collapses to one signature. Without this the guard only caught byte-identical
/// repetition, and a fixating model would tweak the tail every turn and burn all
/// `max_iterations` (observed: 17 turns re-running one `git show | awk | grep`
/// pipeline with a changing tail, each producing a slightly different count).
pub(super) fn tool_signature(calls: &[cowboy_core::model::ToolCall]) -> String {
    signature(calls, true)
}

/// The *raw* signature: name + arguments with no normalization. Byte-identical
/// calls compare equal; a cosmetic edit does not. The loop guard uses the
/// difference between this and [`tool_signature`] to tell exact repetition
/// (polling) from cosmetic churn.
pub(super) fn raw_tool_signature(calls: &[cowboy_core::model::ToolCall]) -> String {
    signature(calls, false)
}

/// Shared body for the two signatures. `normalize` folds cosmetic churn in
/// `shell` commands; without it the arguments are compared verbatim.
fn signature(calls: &[cowboy_core::model::ToolCall], normalize: bool) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|c| {
            let args = if normalize && c.name == "shell" {
                normalize_shell_args(&c.arguments)
            } else {
                c.arguments.clone()
            };
            format!("{}\u{0}{}", c.name, args)
        })
        .collect();
    parts.sort();
    parts.join("\u{1}")
}

/// Normalize a `shell` tool's JSON arguments for loop-guard comparison: parse out
/// the `command`, strip cosmetic tails that don't change *what is being
/// inspected*, and re-emit. Falls back to the raw string if the JSON or the
/// `command` field isn't shaped as expected — a normalization miss only makes the
/// guard slightly less sensitive, never wrong.
///
/// Deliberately conservative: it strips only additive noise (trailing counters,
/// `echo` narration, stderr-merge, whitespace), never rewrites the substantive
/// pipeline. Over-normalizing would collapse *legitimate* iterative refinement
/// into a false loop and abort real work.
fn normalize_shell_args(arguments: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.to_string();
    };
    let Some(cmd) = v.get("command").and_then(|c| c.as_str()) else {
        return arguments.to_string();
    };
    format!("command={}", normalize_shell_command(cmd))
}

/// The cosmetic-churn stripper. Splits the command on the segment separators a
/// model uses to append probes (`;`, `&&`, `|`), drops segments that are pure
/// narration or counting (`echo …`, `wc -l/-c`, `true`), strips a trailing
/// `2>&1`, collapses whitespace, and rejoins. What remains is the substantive
/// work; if that is unchanged across turns the model is not making progress.
fn normalize_shell_command(cmd: &str) -> String {
    // Drop shell line-continuations (`\` + newline) so a reflowed command doesn't
    // leave a stray backslash token, then collapse whitespace runs (incl.
    // newlines) to single spaces so reflowed-but-identical commands compare equal.
    let joined = cmd.replace("\\\n", " ");
    let flat = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    // Strip a trailing stderr-merge, a common no-op tweak.
    let flat = flat.strip_suffix(" 2>&1").unwrap_or(&flat).trim();

    // Split into segments on `;`, `&&`, `||`, and `|` so we can drop the
    // cosmetic ones. This is a coarse split (it ignores quoting), which is fine:
    // the result feeds a similarity hash, not an executor.
    let segments = flat
        .split(&[';', '|'][..])
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let kept: Vec<String> = segments
        .filter(|seg| !is_cosmetic_segment(seg))
        .map(|seg| {
            // `grep -c PAT` (count) and `grep PAT` (print) inspect the same thing;
            // fold the count flag away so switching between them isn't "progress".
            seg.replace("grep -c ", "grep ")
                .replace("grep -cE ", "grep -E ")
                .replace("grep -ci ", "grep -i ")
        })
        .collect();

    kept.join("|")
}

/// A command segment that only reports/echoes and doesn't change what is being
/// inspected: `echo …`, `wc -l/-c/-m`, a bare `wc`, `head`/`tail` line-count
/// tweaks, and shell no-ops. These are exactly the tails a stuck model appends
/// turn over turn.
fn is_cosmetic_segment(seg: &str) -> bool {
    let head = seg.split_whitespace().next().unwrap_or("");
    matches!(head, "echo" | "printf" | "true" | ":") || seg == "wc" || seg.starts_with("wc -")
}

/// Truncate `output` to at most `max_bytes`, on a char boundary, with a marker.
///
/// Head-only. Use [`truncate_middle`] for command output, where the tail carries
/// the result; this stays the predictable structural backstop in
/// `push_tool_result`, and is right for anything whose front matters most (a
/// `read` window, which is ordered — see [`read_continuation_hint`] for what
/// replaces the continuation hint the cut takes with it).
pub(super) fn truncate(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[... output truncated at {} bytes ...]",
        &output[..end],
        max_bytes
    )
}

/// Truncate `output` to at most `max_bytes`, keeping **both ends**, with a marker
/// in the middle naming how much was dropped.
///
/// Head-only truncation discards the most valuable part of build and test output:
/// the last lines, which carry the verdict — `test result: FAILED. 3 passed; 2
/// failed`, the linker error, the panic, the failing assertion. A verbose
/// `cargo test` that overran the cap used to hand the model 60k of compile
/// progress with every failure cut off the end, so the obvious next move was to
/// re-run the same command to find out what broke — paying for the output twice
/// and learning nothing the first time.
///
/// Split evenly between the ends. The head keeps whatever the caller put in front
/// (the `[exit code: N]` line) plus the first errors; the tail keeps the summary.
/// Cuts prefer a nearby line boundary so neither end is a half line, and fall back
/// to a char boundary when there is no newline to snap to (minified output, one
/// enormous line).
///
/// Degrades to [`truncate`] when `max_bytes` is too small to hold two useful
/// fragments plus the marker — at that size a single head is the honest answer.
pub(super) fn truncate_middle(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    // The marker itself has to fit, and two fragments plus a marker only beat a
    // single head once there is real room for both.
    const MIN_SPLIT_BYTES: usize = 512;
    if max_bytes < MIN_SPLIT_BYTES {
        return truncate(output, max_bytes);
    }
    // Reserve generously for the marker; a slightly smaller body is fine, a body
    // that pushes the total over `max_bytes` is not (`push_tool_result` would then
    // head-truncate the result and cut off the tail this exists to preserve).
    const MARKER_RESERVE: usize = 96;
    let body = max_bytes.saturating_sub(MARKER_RESERVE);
    let head_budget = body / 2;
    let tail_budget = body - head_budget;

    let head_end = snap_back(output, head_budget);
    let tail_start = snap_forward(output, output.len() - tail_budget);
    // Snapping moved the cuts; if they crossed or met there is nothing to elide and
    // a plain head is correct.
    if tail_start <= head_end {
        return truncate(output, max_bytes);
    }
    let elided = tail_start - head_end;
    format!(
        "{}\n[... {} bytes elided ({} of {} shown; the middle was dropped, both ends kept) ...]\n{}",
        &output[..head_end],
        elided,
        output.len() - elided,
        output.len(),
        &output[tail_start..]
    )
}

/// Largest index `<= at` that is a char boundary, preferring the end of a line
/// within [`SNAP_SLACK`] bytes so a fragment does not stop mid-line.
fn snap_back(s: &str, at: usize) -> usize {
    let mut end = at.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let floor = end.saturating_sub(SNAP_SLACK);
    match s[floor..end].rfind('\n') {
        // +1 keeps the newline with the head, so the marker starts on its own line.
        Some(i) => floor + i + 1,
        None => end,
    }
}

/// Smallest index `>= at` that is a char boundary, preferring the start of a line
/// within [`SNAP_SLACK`] bytes so a fragment does not start mid-line.
fn snap_forward(s: &str, at: usize) -> usize {
    let mut start = at.min(s.len());
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let ceil = (start + SNAP_SLACK).min(s.len());
    match s[start..ceil].find('\n') {
        Some(i) => start + i + 1,
        None => start,
    }
}

/// How far [`snap_back`]/[`snap_forward`] will travel to find a line boundary.
/// Bounded so a single enormous line cannot shrink a fragment to nothing.
const SNAP_SLACK: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::model::ToolCall;

    /// A roster from YAML: exercises the real deserialization (and its defaults)
    /// instead of hand-building a struct, and keeps these tests independent of the
    /// host's `~/.config/cowboy/crew.yaml`.
    fn roster(yaml: &str) -> cowboy_core::crew::CrewConfig {
        serde_yaml_ng::from_str(yaml).expect("test roster parses")
    }

    #[test]
    fn liveness_is_only_trusted_in_the_direction_that_is_certain() {
        // A missing `/proc/<pid>` means the process is definitely gone; that is the only
        // conclusion this draws, and it is the one the reaping logic needs.
        assert!(!process_is_gone(std::process::id()));
        // Above any possible `pid_max` (which is capped at 2^22), so this cannot exist.
        assert!(process_is_gone(u32::MAX));
    }

    #[test]
    fn depth_limit_honours_the_roster_but_never_exceeds_the_ceiling() {
        // No roster: the hard ceiling.
        assert_eq!(effective_max_depth(None), MAX_SUBAGENT_DEPTH);
        // A shallower roster limit wins.
        let shallow = roster("crew:\n  general: cheap\ndelegation:\n  max_depth: 1\n");
        assert_eq!(effective_max_depth(Some(&shallow)), 1);
        // A roster asking for more than the ceiling is clamped to it.
        let deep = roster("crew:\n  general: cheap\ndelegation:\n  max_depth: 99\n");
        assert_eq!(effective_max_depth(Some(&deep)), MAX_SUBAGENT_DEPTH);
        // Recursive delegation ignores max_depth and takes the ceiling.
        let recursive = roster(
            "crew:\n  general: cheap\ndelegation:\n  max_depth: 1\n  \
             allow_recursive_delegation: true\n",
        );
        assert_eq!(effective_max_depth(Some(&recursive)), MAX_SUBAGENT_DEPTH);
    }

    #[test]
    fn delegation_needs_crew_mode_and_headroom() {
        let cfg = roster("crew:\n  general: cheap\ndelegation:\n  max_depth: 1\n");
        // Solo mode: never, at any depth.
        assert!(!delegation_available(false, 0, Some(&cfg)));
        // Crew mode with headroom: yes.
        assert!(delegation_available(true, 0, Some(&cfg)));
        // At the limit: no — this is the case that used to cost a round-trip.
        assert!(!delegation_available(true, 1, Some(&cfg)));
        // Past the limit (defensive): still no.
        assert!(!delegation_available(true, 5, Some(&cfg)));
    }

    #[test]
    fn a_loop_that_cannot_delegate_is_offered_neither_the_tool_nor_the_guidance() {
        let names = |ts: &[ToolDef]| -> Vec<String> { ts.iter().map(|t| t.name.clone()).collect() };
        let foreman_tools = names(&tool_surface(true, false));
        for t in [
            tools::TOOL_SUBAGENT,
            tools::TOOL_JOBS,
            tools::TOOL_WAIT,
            tools::TOOL_JOB_REPLY,
        ] {
            assert!(foreman_tools.iter().any(|n| n == t), "missing {t}");
        }
        // A foreman has no foreman of its own to ask for turns.
        assert!(!foreman_tools.iter().any(|n| n == tools::TOOL_REQUEST_TURNS));

        // A worker that cannot delegate gets none of the supervision tools — offering
        // `jobs`/`wait` to a loop with no jobs is just noise it can waste a turn on.
        let leaf_tools = names(&tool_surface(false, true));
        for t in [
            tools::TOOL_SUBAGENT,
            tools::TOOL_JOBS,
            tools::TOOL_WAIT,
            tools::TOOL_JOB_REPLY,
        ] {
            assert!(!leaf_tools.iter().any(|n| n == t), "should not offer {t}");
        }
        assert!(leaf_tools.iter().any(|n| n == tools::TOOL_REQUEST_TURNS));

        // A solo run gets neither side.
        let solo = names(&tool_surface(false, false));
        assert!(!solo.iter().any(|n| n == tools::TOOL_REQUEST_TURNS));
        assert!(!solo.iter().any(|n| n == tools::TOOL_SUBAGENT));

        // The prompt must agree with the tool surface: telling a worker to delegate
        // with no `subagent` tool is what produced "Delegation isn't available at this
        // depth".
        let foreman = system_prompt(true, 0, false, None);
        assert!(foreman.contains("foreman of a crew"));
        assert!(!foreman.contains(SUBAGENT_PROMPT));
        assert!(!foreman.contains(TURN_REQUEST_PROMPT));

        let leaf = system_prompt(false, MAX_SUBAGENT_DEPTH, true, None);
        assert!(!leaf.contains("foreman of a crew"));
        // …but it still gets the subagent-specific guidance, since it *is* one.
        assert!(leaf.contains(SUBAGENT_PROMPT));
        assert!(leaf.contains(TURN_REQUEST_PROMPT));

        // A worker with a grant but no channel to ask on is not told to ask.
        let unsupervised_leaf = system_prompt(false, 1, false, None);
        assert!(!unsupervised_leaf.contains(TURN_REQUEST_PROMPT));

        let solo_prompt = system_prompt(false, 0, false, None);
        assert!(!solo_prompt.contains("foreman of a crew"));
        assert!(!solo_prompt.contains(SUBAGENT_PROMPT));
    }

    #[test]
    fn the_prompt_names_the_sandboxs_real_paths() {
        let base = system_prompt(false, 0, false, None);
        assert!(base.contains("/workspace") && base.contains("/tmp"));
        let same = with_sandbox_paths(base.clone(), &crate::sandbox::SandboxPaths::default());
        assert_eq!(same, base, "the Linux defaults leave the prompt alone");

        let mac = with_sandbox_paths(
            base,
            &crate::sandbox::SandboxPaths {
                workdir: "/Users/dev/proj".into(),
                scratch: "/Users/dev/.cache/cowboy/run/scratch/s/tmp".into(),
            },
        );
        assert!(!mac.contains("/workspace"), "{mac}");
        assert!(!mac.contains(" /tmp"), "{mac}");
        assert!(mac.contains("mounted at /Users/dev/proj"), "{mac}");
        assert!(
            mac.contains("go under /Users/dev/.cache/cowboy/run/scratch/s/tmp"),
            "{mac}"
        );
    }

    #[test]
    fn a_path_token_is_replaced_only_whole() {
        assert_eq!(
            replace_path_token("/tmp and /tmpfoo", "/tmp", "/x"),
            "/x and /tmpfoo"
        );
        assert_eq!(replace_path_token("at /tmp.", "/tmp", "/x"), "at /x.");
    }

    /// The foreman must be told about exactly the categories that can route, with a
    /// stated meaning for each. The bug this guards: a hardcoded list omitted
    /// `review` while the tool advertised it, so review work silently fell back to
    /// `general` and the roster's review slots were never used.
    #[test]
    fn the_foreman_is_told_every_roster_category_and_what_it_means() {
        use cowboy_core::crew::{builtin_description, CrewConfig, Ramp};
        use std::collections::BTreeMap;

        let mut crew = BTreeMap::new();
        for c in ["general", "review", "exploration", "perf"] {
            crew.insert(c.to_string(), Ramp::Single("m".into()));
        }
        let mut descriptions = BTreeMap::new();
        // A category Cowboy has never heard of, defined by the user.
        descriptions.insert(
            "perf".to_string(),
            "profiling; show before/after".to_string(),
        );
        // …and a shipped meaning the user has deliberately overridden.
        descriptions.insert(
            "review".to_string(),
            "read the diff, never edit".to_string(),
        );
        let cfg = CrewConfig {
            version: 1,
            crew,
            temperature: BTreeMap::new(),
            descriptions,
            delegation: Default::default(),
            legacy_planner: None,
        };

        let p = foreman_prompt(Some(&cfg));
        // Every category the roster defines is named…
        for c in ["general", "review", "exploration", "perf"] {
            assert!(
                p.contains(&format!("`{c}`")),
                "category {c} missing from prompt"
            );
        }
        // …the user's own wording wins over the shipped definition…
        assert!(p.contains("read the diff, never edit"));
        assert!(p.contains("profiling; show before/after"));
        // …and a category left undescribed still gets Cowboy's definition.
        assert!(p.contains(builtin_description("exploration").unwrap()));

        // A category NOT in this roster must not be advertised, or the foreman will
        // spend delegations on a route that silently degrades to `general`.
        assert!(!p.contains("`frontend`"));
    }

    /// Effort is the cost dial, so the prompt must state what each level buys on
    /// *this* roster rather than leaving the model to read the adjectives.
    #[test]
    fn the_foreman_gets_this_rosters_actual_turn_grants() {
        let p = foreman_prompt(None);
        let d = cowboy_core::crew::Delegation::default();
        for e in cowboy_core::crew::Effort::all() {
            let grant = d.grant_for(e);
            assert!(
                p.contains(&format!("{grant} turns")) || p.contains(&format!("≈ {grant}")),
                "effort {} missing its grant ({grant}) from the prompt",
                e.as_str()
            );
        }
        assert!(p.contains("difficulty"));
    }

    #[test]
    fn only_pure_coordination_batches_are_exempt_from_the_progress_guards() {
        let call = |name: &str| cowboy_core::model::ToolCall {
            id: "x".into(),
            name: name.into(),
            arguments: "{}".into(),
        };
        assert!(is_coordination_only(&[call(tools::TOOL_JOBS)]));
        assert!(is_coordination_only(&[
            call(tools::TOOL_WAIT),
            call(tools::TOOL_JOB_REPLY)
        ]));
        // A batch that also does real work is judged like any other.
        assert!(!is_coordination_only(&[
            call(tools::TOOL_JOBS),
            call(tools::TOOL_SHELL)
        ]));
        assert!(!is_coordination_only(&[call(tools::TOOL_READ)]));
        // Dispatching is work, not coordination.
        assert!(!is_coordination_only(&[call(tools::TOOL_SUBAGENT)]));
        assert!(!is_coordination_only(&[]));
    }

    #[test]
    fn a_delegated_worker_is_supervised_and_a_foreman_is_not() {
        // Parent set both: supervised, on its grant.
        let b = IterationBudget::resolve(Some(60), Some(400), 100);
        assert_eq!((b.granted, b.ceiling, b.used), (60, 400, 0));
        assert!(b.supervised);

        // No env (the foreman): plain max_iterations, nobody to ask.
        let mut f = IterationBudget::resolve(None, None, 100);
        assert_eq!((f.granted, f.ceiling), (100, 100));
        assert!(!f.supervised);
        assert_eq!(f.extend(50), 0, "an unsupervised loop cannot be extended");
    }

    #[test]
    fn a_broken_or_half_present_budget_falls_back_rather_than_stranding_the_worker() {
        // Zero grant would mean "no turns at all" — a config typo must not silently
        // make delegation do nothing.
        for (grant, ceiling) in [
            (Some(0), Some(400)),
            (Some(60), Some(0)),
            (Some(60), None),
            (None, Some(400)),
            (None, None),
        ] {
            let b = IterationBudget::resolve(grant, ceiling, 100);
            assert_eq!(b.granted, 100, "grant={grant:?} ceiling={ceiling:?}");
            assert!(!b.supervised);
        }
    }

    #[test]
    fn a_grant_larger_than_the_ceiling_is_clamped_on_the_way_in() {
        let b = IterationBudget::resolve(Some(500), Some(50), 100);
        assert_eq!(b.granted, 50);
        assert_eq!(b.ceiling, 50);
    }

    #[test]
    fn extensions_stop_at_the_ceiling_and_report_how_many_landed() {
        let mut b = IterationBudget::resolve(Some(60), Some(100), 100);
        assert_eq!(b.extend(20), 20);
        assert_eq!(b.granted, 80);
        // Asking for more than the headroom grants only the headroom …
        assert_eq!(b.extend(50), 20);
        assert_eq!(b.granted, 100);
        // … and at the ceiling nothing lands, which the caller must report rather
        // than looping on a request that can never be satisfied.
        assert_eq!(b.extend(10), 0);
        assert_eq!(b.granted, 100);
    }

    #[test]
    fn remaining_and_exhausted_track_use() {
        let mut b = IterationBudget::resolve(Some(3), Some(10), 100);
        assert_eq!(b.remaining(), 3);
        b.used = 3;
        assert!(b.exhausted());
        assert_eq!(b.remaining(), 0);
        // A grant extension un-exhausts it.
        b.extend(2);
        assert!(!b.exhausted());
        assert_eq!(b.remaining(), 2);
    }

    #[test]
    fn the_depletion_nudge_fires_at_seventy_then_ninety_percent() {
        // A 100-turn grant: quiet until 70, nudge, then urgent from 90.
        assert_eq!(grant_stage(1, 100), GrantStage::Fine);
        assert_eq!(grant_stage(69, 100), GrantStage::Fine);
        assert_eq!(grant_stage(70, 100), GrantStage::Nudge);
        assert_eq!(grant_stage(89, 100), GrantStage::Nudge);
        assert_eq!(grant_stage(90, 100), GrantStage::Urgent);
        assert_eq!(grant_stage(100, 100), GrantStage::Urgent);
        // Proportional, so a small grant gets the same warning shape.
        assert_eq!(grant_stage(10, 15), GrantStage::Fine);
        assert_eq!(grant_stage(11, 15), GrantStage::Nudge);
        assert_eq!(grant_stage(14, 15), GrantStage::Urgent);
        // Stages are ordered so the loop can fire each exactly once.
        assert!(GrantStage::Urgent > GrantStage::Nudge);
        assert!(GrantStage::Nudge > GrantStage::Fine);
    }

    #[test]
    fn the_nudge_only_mentions_asking_when_there_is_someone_to_ask() {
        let supervised = IterationBudget::resolve(Some(60), Some(400), 100);
        let solo = IterationBudget::resolve(None, None, 100);
        assert!(grant_notice(GrantStage::Fine, &supervised).is_none());

        let s = grant_notice(GrantStage::Urgent, &supervised).unwrap();
        assert!(s.contains("request_turns"), "got: {s}");
        // A foreman has no foreman: pointing it at `request_turns` would be advice it
        // cannot act on.
        let f = grant_notice(GrantStage::Urgent, &solo).unwrap();
        assert!(!f.contains("request_turns"), "got: {f}");
        assert!(f.contains("final"), "got: {f}");
    }

    fn read_call(path: &str) -> ToolCall {
        ToolCall {
            id: "r".into(),
            name: tools::TOOL_READ.into(),
            arguments: serde_json::json!({ "path": path }).to_string(),
        }
    }

    #[test]
    fn an_unchanged_reread_is_elided_but_a_changed_one_is_not() {
        let mut t = ProgressTracker::default();
        let key = ProgressTracker::read_key("src/main.rs", None, None);
        // First read: novel, content passes through.
        assert_eq!(t.note_read(&key, "fn main() {}", 1), None);
        // Same content again: point at step 1 instead of spending context twice.
        assert_eq!(t.note_read(&key, "fn main() {}", 4), Some(1));
        assert_eq!(t.note_read(&key, "fn main() {}", 5), Some(1));
        // The file changed (the agent edited it): the new content must get through,
        // or the agent would be reasoning about a stale copy.
        assert_eq!(t.note_read(&key, "fn main() { work() }", 6), None);
        // …and the *new* content is what a later re-read is compared against.
        assert_eq!(t.note_read(&key, "fn main() { work() }", 7), Some(1));
    }

    #[test]
    fn a_windowed_read_is_a_different_observation_from_a_full_one() {
        let full = ProgressTracker::read_key("a.rs", None, None);
        let windowed = ProgressTracker::read_key("a.rs", Some(100), Some(50));
        assert_ne!(full, windowed);
        let mut t = ProgressTracker::default();
        assert_eq!(t.note_read(&full, "whole file", 1), None);
        // Reading a different part of the same file is real work, not a re-read.
        assert_eq!(t.note_read(&windowed, "just lines 100-150", 2), None);
    }

    #[test]
    fn the_observed_reread_loop_goes_barren_immediately() {
        // The failure from the field: the same file read over and over, each read
        // returning the same bytes, for ~90 iterations. The batch-signature guards
        // can miss this; the novelty metric cannot.
        //
        // Mirrors the loop's order: the read completes (`note_read`), then the
        // iteration is classified (`observe`).
        let mut t = ProgressTracker::default();
        let path = "crates/riffdb-storage-redb/src/changelog_v3_cursor.rs";
        let key = ProgressTracker::read_key(path, None, None);
        let body = "fn next_receipt() {}";

        t.note_read(&key, body, 1);
        assert!(
            !t.observe(&[read_call(path)], false).is_barren(),
            "the first read is new information"
        );
        for i in 0..5 {
            t.note_read(&key, body, i + 2);
            let n = t.observe(&[read_call(path)], false);
            assert!(n.is_barren(), "re-read {i} should be barren: {n:?}");
            assert_eq!(n.rereads, 1);
            assert_eq!(n.new_reads, 0);
        }
        assert_eq!(t.barren_streak(), 5);
        assert!(t.stalled(4));
        assert!(!t.stalled(6));
        // A window of 0 disables the check entirely.
        assert!(!t.stalled(0));
    }

    #[test]
    fn a_changed_file_reread_counts_as_new_information() {
        let mut t = ProgressTracker::default();
        let key = ProgressTracker::read_key("a.rs", None, None);
        t.note_read(&key, "before", 1);
        t.observe(&[read_call("a.rs")], false);
        // The agent edited the file and read it back: not a wasted step.
        t.note_read(&key, "after", 2);
        let n = t.observe(&[read_call("a.rs")], false);
        assert!(!n.is_barren(), "{n:?}");
        assert_eq!(n.new_reads, 1);
        assert_eq!(n.rereads, 0);
    }

    #[test]
    fn legitimate_iterative_work_is_never_barren() {
        let mut t = ProgressTracker::default();
        // Reading a *different* file each time is exploration, not circling — that
        // failure mode is the iteration grant's job, not this metric's.
        for f in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            t.note_read(&ProgressTracker::read_key(f, None, None), "body", 1);
            assert!(!t.observe(&[read_call(f)], false).is_barren());
        }
        assert_eq!(t.barren_streak(), 0);

        // Polling: the same command, byte-identical, but the output keeps changing
        // (a build log, a health check). The existing guard deliberately allows this,
        // and so must this one.
        let poll = ToolCall {
            id: "s".into(),
            name: tools::TOOL_SHELL.into(),
            arguments: serde_json::json!({ "command": "cargo build 2>&1 | tail -5" }).to_string(),
        };
        assert!(
            !t.observe(std::slice::from_ref(&poll), false).is_barren(),
            "new command"
        );
        for _ in 0..3 {
            let n = t.observe(std::slice::from_ref(&poll), true);
            assert!(!n.is_barren(), "changed output is progress: {n:?}");
        }
        assert_eq!(t.barren_streak(), 0);
        // The same command with unchanged output *is* barren.
        assert!(t.observe(&[poll], false).is_barren());
    }

    #[test]
    fn an_edit_is_always_progress() {
        let mut t = ProgressTracker::default();
        let edit = ToolCall {
            id: "e".into(),
            name: tools::TOOL_EDIT.into(),
            arguments: serde_json::json!({ "path": "a.rs", "old_str": "x", "new_str": "y" })
                .to_string(),
        };
        // Even repeated on the same file: an edit changes the world.
        for _ in 0..3 {
            assert!(!t.observe(std::slice::from_ref(&edit), false).is_barren());
        }
        assert!(t.evidence().contains("files edited: 3"));
    }

    #[test]
    fn bookkeeping_calls_neither_count_as_progress_nor_as_circling() {
        let mut t = ProgressTracker::default();
        let plan = ToolCall {
            id: "p".into(),
            name: tools::TOOL_PLAN.into(),
            arguments: serde_json::json!({ "steps": ["a"] }).to_string(),
        };
        // Updating a plan is barren (nothing was learned) …
        assert!(t.observe(std::slice::from_ref(&plan), false).is_barren());
        // … but a real read alongside it is not.
        t.note_read(&ProgressTracker::read_key("a.rs", None, None), "body", 2);
        assert!(!t.observe(&[plan, read_call("a.rs")], false).is_barren());
    }

    #[test]
    fn a_redirect_can_clear_the_streak() {
        let mut t = ProgressTracker::default();
        let key = ProgressTracker::read_key("a.rs", None, None);
        t.note_read(&key, "body", 1);
        t.observe(&[read_call("a.rs")], false);
        for _ in 0..3 {
            t.note_read(&key, "body", 2);
            t.observe(&[read_call("a.rs")], false);
        }
        assert_eq!(t.barren_streak(), 3);
        t.clear_streak();
        assert_eq!(t.barren_streak(), 0);
    }

    #[test]
    fn the_evidence_block_reports_what_was_measured() {
        let mut t = ProgressTracker::default();
        let key = ProgressTracker::read_key("a.rs", None, None);
        t.note_read(&key, "body", 1);
        t.observe(&[read_call("a.rs")], false);
        t.note_read(&key, "body", 2);
        t.observe(&[read_call("a.rs")], false);
        let e = t.evidence();
        assert!(e.contains("files read: 1"), "got: {e}");
        assert!(e.contains("unchanged re-reads: 1"), "got: {e}");
        assert!(e.contains("nothing new: 1"), "got: {e}");
    }

    #[test]
    fn the_reread_notice_says_what_to_do_instead() {
        let n = reread_notice("src/main.rs", 7);
        assert!(n.contains("src/main.rs"));
        assert!(n.contains("step 7"));
        // A bare refusal invites a retry; this has to name the alternatives.
        assert!(n.contains("offset"), "got: {n}");
        assert!(n.contains("grep"), "got: {n}");
    }

    fn shell(cmd: &str) -> Vec<ToolCall> {
        vec![ToolCall {
            id: "x".into(),
            name: "shell".into(),
            arguments: serde_json::json!({ "command": cmd }).to_string(),
        }]
    }

    #[test]
    fn whitespace_and_stderr_merge_are_cosmetic() {
        let a = tool_signature(&shell("git show HEAD | grep -E foo"));
        let b = tool_signature(&shell("git  show   HEAD  |  grep -E foo 2>&1"));
        let c = tool_signature(&shell("git show HEAD \\\n | grep -E foo"));
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn appended_echo_and_wc_probes_are_cosmetic() {
        let base = tool_signature(&shell("git show HEAD | grep -E foo"));
        // A trailing counter …
        assert_eq!(
            base,
            tool_signature(&shell("git show HEAD | grep -E foo | wc -l"))
        );
        // … an appended echo narration …
        assert_eq!(
            base,
            tool_signature(&shell("git show HEAD | grep -E foo; echo \"done\""))
        );
        // … and both at once.
        assert_eq!(
            base,
            tool_signature(&shell(
                "git show HEAD | grep -E foo | wc -l; echo \"exit=$?\""
            ))
        );
    }

    #[test]
    fn grep_count_flag_folds_to_the_same_inspection() {
        let print = tool_signature(&shell("git show HEAD | grep -E foo"));
        let count = tool_signature(&shell("git show HEAD | grep -cE foo"));
        assert_eq!(print, count);
    }

    #[test]
    fn a_genuinely_different_command_keeps_a_distinct_signature() {
        // Changing the *substance* (the file, the pattern) must NOT collapse —
        // that would abort legitimate iterative refinement.
        let a = tool_signature(&shell("git show HEAD -- ranch.rs | grep -E foo"));
        let b = tool_signature(&shell("git show HEAD -- scope.rs | grep -E foo"));
        let c = tool_signature(&shell("git show HEAD -- ranch.rs | grep -E bar"));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn non_shell_arguments_are_left_verbatim() {
        // Only `shell` is normalized; other tools compare on raw arguments.
        let a = vec![ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: r#"{"path":"/a  b"}"#.into(),
        }];
        let b = vec![ToolCall {
            id: "2".into(),
            name: "read".into(),
            arguments: r#"{"path":"/a b"}"#.into(),
        }];
        assert_ne!(tool_signature(&a), tool_signature(&b));
    }

    #[test]
    fn the_observed_churn_burst_collapses_to_one_signature() {
        // Verbatim tails from session 1788790179664-2 turns 85–100: one
        // `git show … | awk … | grep …` pipeline the model kept re-issuing with a
        // changing cosmetic tail. Pre-fix each had a distinct signature and slipped
        // the guard; post-fix they must all collapse so the guard fires.
        let core = "git show 21c9a35 --format=\"\" -- crates/cowboy-cli/src/cmd/ranch.rs \
                    | awk '/^@@ -183/{p=1} /^@@ -376/{exit} p' | grep -E \"^[+-]\" \
                    | grep -vE \"^[+-][+-]\" | grep -E \"test|dead-sid\"";
        let variants = [
            format!("cd /workspace && {core} | wc -l"),
            format!("cd /workspace && {core} | wc -l; echo \"exit=$?\""),
            format!("cd /workspace && {core} | wc -l 2>&1; echo done"),
            format!("cd /workspace && {core}   |   wc -l"),
        ];
        let first = tool_signature(&shell(&variants[0]));
        for v in &variants[1..] {
            assert_eq!(
                first,
                tool_signature(&shell(v)),
                "variant should collapse: {v}"
            );
        }
    }
}
