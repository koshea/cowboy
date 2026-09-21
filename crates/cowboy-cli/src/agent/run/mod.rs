//! The Cowboy-owned agent loop: model turn -> tool call -> observation ->
//! repeat, until `final`, `ask_user` is answered, or limits are hit. Cowboy
//! owns this lifecycle; no agent framework.

use anyhow::Result;
use cowboy_core::config::AgentBehavior;
use cowboy_core::model::{ChatResponse, Delta, Message, ModelClient, Role, ToolDef};
use tokio_util::sync::CancellationToken;

use super::tools::{
    self, ArtifactArgs, AskUserArgs, BlockedArgs, DecisionArgs, EditArgs, FinalArgs, GrepArgs,
    HandoffArgs, McpArgs, MemoryArgs, PlanArgs, ProposeScopeChangeArgs, ReadArgs, RequestPathArgs,
    ShellArgs, SubagentArgs, WriteArgs,
};
use super::ui::{AgentUi, ContextUsage};
use crate::sandbox::{ExecResult, Sandbox};
use crate::session::SessionLogger;

/// Process-unique counter so concurrent subagents spawned in the same millisecond
/// get distinct session ids.
static SUBAGENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

mod handlers;
mod support;
use support::{
    applied_change_note, fmt_duration, read_continuation_hint, shell_outcome_note, SeenFiles,
    SeenStatus, Trim,
};
use support::{
    delegation_available, effective_max_depth, emit_delta, fileop_summary, grant_notice,
    grant_stage, is_coordination_only, parse_args, process_is_gone, raw_tool_signature,
    render_plan, render_transcript, reread_notice, self_exe, system_prompt, tool_signature,
    tool_surface, truncate, truncate_middle, unified_diff, GrantStage, IterationBudget,
    ProgressTracker, Verification,
};

/// Default agent system prompt (see plan §10.3).
pub const SYSTEM_PROMPT: &str = "\
You are Cowboy, an autonomous coding agent running inside a locked-down sandbox \
on the user's machine.

The project is mounted at /workspace. You may freely inspect, edit, build, test, \
and run code inside the sandbox. Use `shell` for builds, tests, git, and other \
commands. For files, prefer the structured tools: `read` (with line numbers), \
`grep` (search the workspace for a regex — use it instead of `grep -r`/`rg`, \
which may not be installed and will drown you in build output), `edit` (exact \
unique-string replacement, or a batch of `edits` applied all-or-nothing), and \
`write` (create/overwrite) — they are more reliable and cheaper than \
`cat`/`sed`/heredocs.

Cowboy-specific helpers are CLIs you invoke through `shell`, e.g. `cowboy patch \
show`. You do not need to ask before ordinary development actions inside the \
sandbox. Each `shell` call is a fresh process: `cd` and `export` do not carry to \
the next one — pass `cwd` or chain with `&&`. A server or watcher that never \
exits must NOT be run in the foreground; start it with `proc` and test against it.

Reusable skills are listed below when this project has any; read one with `cowboy \
skill show <name>` before doing that kind of work and then follow it (skills are \
discovered from `.cowboy/skills/` and `.claude/skills/`).

Project conventions live in AGENTS.md (or CLAUDE.md) files, which are \
authoritative. The repo-root one is included below when present; when you work in \
a subtree, also `read` the nearest AGENTS.md on the path to the files you're \
touching (the nearest one wins). When you establish — or the user tells you — a \
durable project convention (build/test commands, style rules, layout), record it \
in the appropriate AGENTS.md with `edit`/`write` so it persists for everyone.

You also have a private cross-session `memory` (stored on the host, not the \
repo). The index of what you've saved is shown below when present; `recall` a \
full entry by name when it's relevant, and `save` concise facts or user \
preferences worth remembering next time (default scope \"project\"; \"global\" \
applies across all projects and requires the user to approve the save). Keep \
project conventions in AGENTS.md, not memory.

The runtime enforces network, host, and secret permissions outside your control. \
Outbound network access goes through a gateway that allows, denies, or prompts \
the user per destination. A blocked request surfaces as a connection/TLS error \
(e.g. \"connection reset\", \"TLS closed\", curl exit 35/35) — this means the \
host has not approved that destination, NOT that the destination is down. Do not \
retry the same blocked host with different tools or flags; instead state plainly \
which host:port you need and why, and let the user approve it (or proceed without \
network). If a command cannot access something, observe the failure and continue.

Your filesystem view is also enforced outside your control: you see the project \
and the host toolchain, not the whole machine. A path outside the project reads as \
\"No such file or directory\" or \"Permission denied\" even when it exists. If you \
genuinely need one — a sibling repository, a dataset, a shared toolchain — use \
`request_path` with the path and a specific reason; the user approves or denies. \
On approval it is visible to the NEXT command, so re-run the one that failed. Do \
not retry the same path with different tools first, and do not ask twice for a \
path that was denied. Credential stores (~/.aws, ~/.ssh, ~/.gnupg, browser \
profiles) are always refused: if a task needs credentials, say so and tell the \
user about `cowboy secrets add`.

For a multi-step task, use the `plan` tool to keep a short, visible checklist: \
lay out the steps up front, keep exactly one step \"in_progress\" at a time, and \
mark steps \"done\" as you complete them (re-send the whole list to update it). \
Before you finish, send the plan one last time with EVERY step marked \"done\" \
(or dropped if abandoned) — never leave a step \"in_progress\"/\"pending\" when \
you call `final`. Skip the plan tool entirely for trivial one-step work.

Before large edits, inspect the repository and form a brief plan. After edits, run \
relevant checks. Publish durable outputs others may need with `artifact` (e.g. an \
API/schema contract). At the end of a substantial task, write a `handoff` (goal, \
status, changed files, decisions, contracts, validation, risks, next steps) so the \
next worker can continue, then call `final` summarizing what changed, what was \
validated, and remaining risks or follow-up work.";

/// The crew-foreman delegation guidance, appended to the system prompt only in crew
/// mode (a roster exists and delegation is enabled) **and** only for a loop that can
/// actually delegate. In solo mode — or in a worker already at the depth limit — this
/// isn't shown and the `subagent` tool isn't offered.
/// Build the foreman guidance for a given roster.
///
/// The category list and the effort anchors are generated from the *user's* roster
/// rather than hardcoded. A hardcoded list had drifted: it named nine categories
/// and omitted `review`, while the `subagent` tool description advertised `review`
/// as an example — so review work was routed to a category the foreman had never
/// been told existed, fell through to `general`, and the roster's review slots were
/// never used. Generating it means the two can never disagree again, and a user who
/// adds a category to `crew.yaml` gets a foreman that knows about it.
pub fn foreman_prompt(roster: Option<&cowboy_core::crew::CrewConfig>) -> String {
    use cowboy_core::crew::{builtin_description, Delegation, Effort, GENERAL};

    // Each category is listed WITH its meaning, so the model routes on a stated
    // contract instead of inferring one from the word. The meaning is the user's own
    // if they wrote one in `crew.yaml`, else Cowboy's shipped definition — either way
    // the roster the user authored and the prompt the model reads cannot disagree.
    let named: Vec<(&str, Option<&str>)> = match roster {
        Some(c) if !c.crew.is_empty() => c
            .crew
            .keys()
            .map(|k| (k.as_str(), c.description_for(k)))
            .collect(),
        _ => vec![(GENERAL, builtin_description(GENERAL))],
    };
    let cat_list = named
        .iter()
        .map(|(name, desc)| match desc {
            Some(d) => format!("`{name}` — {d}"),
            None => format!("`{name}`"),
        })
        .collect::<Vec<_>>()
        .join("\n  ");

    // Anchor each effort to the turn grant this roster actually hands out, so the
    // scale means something concrete ("about 25 turns") rather than a bare adjective.
    let d = roster.map(|c| c.delegation.clone()).unwrap_or_default();
    let g = |e: Effort| Delegation::grant_for(&d, e);

    format!(
        "\n\nYou are the foreman of a crew. For focused, separable work, delegate it with the \
`subagent` tool instead of doing everything yourself: describe the work by \
`category` (the kind) and `effort` (how hard), with a `reason` and the \
`expected_artifact`. Do NOT pick a model — Cowboy routes each request to the right \
crew model. To run work in parallel, emit several `subagent` calls in one message. \
Named specialist agents may be defined under `.claude/agents/`/`.cowboy/agents/` \
(`cowboy agents list`); adopt one by passing `agent: <name>` to `subagent`. Delegate \
when work is scoped and separable (exploration, test-writing, an independent \
component, a review pass); do it yourself when the task is tiny, the hand-off costs \
more than the work, or it needs continuous coordination with your current state.\n\n\
CHOOSING `category`. Use exactly one of the categories this user's roster defines, \
and use it as defined here rather than as the word suggests:\n  {cat_list}\n\
Name the work by the artifact it produces, not the subject it touches — test files \
are test work whoever owns the code under test, and reading code to answer a \
question is investigation even when the code it reads is UI. A category outside that \
list is not an error you will be told about — it silently falls back to `general` and \
the roster's routing is wasted, so never invent one. If two categories both seem to \
fit, choose the one whose stated deliverable matches what you actually want back.\n\n\
CHOOSING `effort`. Effort is the difficulty dial, and it sets BOTH the model and the \
worker's turn grant (one turn = one tool call plus the model's reply). On this \
roster: `tiny` ≈ {tiny} turns — one known edit, a lookup, a single file, no \
investigation. `small` ≈ {small} — one obvious change, a little looking around. \
`medium` ≈ {medium} (the default) — one module or concern, real investigation before \
editing. `large` ≈ {large} — a feature or refactor over several files, including \
verifying it. `deep` ≈ {deep} — open-ended work: cause unknown, design cross-cutting, \
worth the strongest model you have. Judge difficulty only: do NOT raise effort to \
signal that something is urgent or important. When torn between two levels pick the \
LOWER one — a worker that needs more can report its progress and ask you for turns, \
whereas an over-sized effort pays a stronger model's rate on every token of a job \
that never needed it.\n\n\
Delegation is ASYNCHRONOUS. `subagent` returns a job id immediately and the worker \
runs in the background — it does NOT return the answer. Keep working while it runs: \
investigate something else, delegate more, or answer the user. Each result is \
delivered to you automatically as a message when that job finishes; you never have \
to poll for it. `jobs` lists what is running (with each worker's turn usage), and \
`wait` parks you until something lands — use it only when you genuinely have nothing \
else to do. You cannot call `final` while jobs are still running: their results are \
part of the task.\n\n\
Size each delegation to fit one worker. A worker starts with a small turn grant \
scaled to its `effort`, so \"review these two crates\" or \"audit the whole repo\" \
will run out mid-way; split that into one job per crate, module, or concern and fan \
them out in parallel instead. A worker that needs more turns will report its progress \
and ask you — you will get its report plus measured evidence (files read, edits made, \
commands run, whether anything new happened) and reply with `job_reply`: `grant` more \
turns when the report shows real progress, `redirect` when it is going the wrong way, \
`wrap_up` to make it write up what it has, or `stop` when the work is no longer \
wanted. Judge the evidence, not the worker's optimism: no new files, no edits and no \
new commands means it is stuck, and more turns will not help.\n\n\
Prefer small, well-scoped subagent tasks that return a concrete artifact. If a \
subagent result comes back prefixed `[partial]`, it ran but did not finish cleanly — \
the text is its work so far plus a session id. Treat that as a checkpoint: \
re-delegate continuing from what's there (pass the prior work as `context`) rather \
than starting the task over.",
        cat_list = cat_list,
        tiny = g(Effort::Tiny),
        small = g(Effort::Small),
        medium = g(Effort::Medium),
        large = g(Effort::Large),
        deep = g(Effort::Deep),
    )
}

/// Extra guidance for a worker running *as* a subagent (depth > 0). Its result is
/// captured from stdout by the foreman, so a single oversized tool call (e.g. a
/// long findings list inlined into one `artifact`/`final`) is dangerous: the
/// model's output-token limit can truncate the arguments mid-string, the call is
/// rejected as malformed, and the whole turn's work is lost. Steer large outputs
/// to a file instead.
pub const SUBAGENT_PROMPT: &str =
    "\n\nYou are running as a subagent: a parent agent dispatched this task and will \
read your final answer. Keep that final answer concise. If your output is large \
(a long list of findings, a big document, lots of structured data), do NOT inline \
it all into a single tool call — model output-token limits can truncate the \
arguments and lose everything. Instead `write` it to a file in the workspace as \
you go, then `publish` it as an artifact by `path` and keep your final answer to a \
short summary that points at the file. Save progress incrementally so partial work \
survives even if you don't finish.";

/// Guidance for a worker that has a turn grant *and* a live channel to ask for more.
/// Kept separate from [`SUBAGENT_PROMPT`] because a worker whose roster disabled
/// supervision has no one to ask, and telling it to ask would be advice it cannot act
/// on.
pub const TURN_REQUEST_PROMPT: &str =
    "\n\nYou have a limited grant of turns for this task, and you will be told when it \
is running low. The grant is deliberately small: if the task turns out to be bigger \
than it, do NOT quietly run out — call `request_turns` with an honest report (what you \
have established, what is left, the next concrete step, how many more turns you need) \
and your foreman will decide. It may grant the turns, redirect you, tell you to write \
up what you have, or stop the work. Cowboy attaches measured evidence of your progress \
to the request — files read, edits made, commands run, and whether the last few steps \
produced anything new — so an accurate report is in your interest, and going in circles \
is visible whatever you say about it. Running out of turns without asking loses the \
work; asking early costs almost nothing.";

/// USD-per-1M-token pricing for the cost estimate. `cached_input` defaults to
/// `input` when the model config names no cache discount.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelPricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cached_input: Option<f64>,
}

impl ModelPricing {
    /// The effective price of a cached input token: the configured cache
    /// price, else the full input price (no assumed discount).
    fn cached_input_or_input(&self) -> Option<f64> {
        self.cached_input.or(self.input)
    }
}

/// Builds a model client by name (host-owned credentials in, built client out),
/// yielding the client, its context window, and its USD pricing. Used to
/// reroute when a model turns out to be unavailable.
pub type ModelBuilder = Box<dyn Fn(&str) -> Result<(Box<dyn ModelClient>, usize, ModelPricing)>>;

/// Drives a single agent session.
pub struct AgentLoop<'a> {
    model: Box<dyn ModelClient>,
    /// Optional dedicated model for auxiliary summarization (compaction +
    /// truncation recovery). `None` falls back to `model` — see [`Self::summarizer`].
    summarizer: Option<Box<dyn ModelClient>>,
    runtime: Box<dyn Sandbox>,
    tools: Vec<ToolDef>,
    behavior: AgentBehavior,
    cancel: CancellationToken,
    /// Model context window (tokens) for history pruning.
    context_window: usize,
    /// Consecutive summarize-and-reprime attempts after a reasoning-budget
    /// truncation, reset to 0 whenever a turn produces content or a tool call.
    /// Bounds recovery so a model that always truncates can't spin.
    reprime_attempts: u32,
    /// Set after a truncation recovery: the next model call asks the provider for
    /// minimal reasoning effort. One turn only — a model that answered is not the
    /// problem, and permanently dulling its thinking would be a poor trade.
    minimize_reasoning_next_turn: bool,
    /// One-shot notice that older reasoning is being shed, so a long session does not
    /// repeat it every turn.
    reasoning_shed_notified: bool,
    /// This turn's iteration budget: a small effort-scaled grant for a delegated
    /// worker (extendable by the foreman, bounded host-side), or plain
    /// `max_iterations` for the foreman. Reset at the start of every turn.
    budget: IterationBudget,
    /// The highest grant-depletion stage already announced this turn, so each nudge
    /// fires once instead of on every iteration past the threshold.
    grant_stage_seen: GrantStage,
    /// What this session has actually read, edited and run — the host's own measure of
    /// whether the worker is making progress, independent of what it claims.
    progress: ProgressTracker,
    /// Host-recorded evidence that the project's checks passed since the last edit.
    /// Inert unless `agent.verify` nominates commands.
    verification: Verification,
    /// What this session has actually seen of each file it touched, so a full-file
    /// `write` over content the agent never read — or that changed under it — is
    /// refused instead of silently winning.
    seen_files: SeenFiles,
    /// Background processes declared in `agent.yaml`, so the `proc` tool can start one
    /// by name without the agent restating its command.
    processes: std::collections::BTreeMap<String, cowboy_core::config::ProcessDef>,
    /// Iterations of zero novelty that trigger a stall intervention (0 = off), from
    /// the roster's `delegation.stall_window`.
    stall_window: u32,
    /// How many stall interventions this turn has needed. Escalates the wording, and
    /// (for a supervised worker) is what turns a second stall into a forced progress
    /// report rather than another directive.
    stall_count: u32,
    /// Background subagent jobs. **Session-scoped**, not turn-scoped: interrupting the
    /// foreman to correct it must not throw away running children, so the registry
    /// outlives `set_cancel` and every turn.
    jobs: crate::agent::jobs::JobRegistry,
    /// Where spawned jobs report Started / TurnRequest / Finished.
    job_tx: tokio::sync::mpsc::UnboundedSender<crate::agent::jobs::JobEvent>,
    job_rx: tokio::sync::mpsc::UnboundedReceiver<crate::agent::jobs::JobEvent>,
    /// Session-wide fan-out cap (`delegation.max_parallel`). Replaces the old
    /// `buffer_unordered` bound, which only capped a single batch — once dispatches
    /// spanned turns, nothing bounded them.
    fanout_sem: std::sync::Arc<tokio::sync::Semaphore>,
    /// Fired to stop every running job. Cloneable and independent of `&mut self`, so
    /// the worker can honour "stop the subagents" while a turn is in flight.
    job_stopper: crate::agent::jobs::JobStopper,
    /// This worker's own channel for asking its foreman for more turns. `None` for a
    /// foreman, and for any worker whose roster disabled supervision.
    control: Option<crate::agent::jobctl::ControlDir>,
    /// How many turn requests this worker has made (the request sequence).
    turn_requests: u32,
    /// How many times the user has extended *this* message's budget.
    user_extensions: u32,
    /// How many unanswered requests were resolved by the automatic extension. The
    /// second one wraps up instead: an unattended foreman must not be able to keep a
    /// worker running forever, nor to destroy its work by never answering.
    auto_extensions: u32,
    /// How long to wait for a verdict before falling back.
    request_timeout: std::time::Duration,
    /// Set once this worker has been told to wrap up (or stop). Terminal: it may spend
    /// the turns it was given to write an answer, but it must not ask again. Without
    /// this latch each wrap-up handed out a few more turns and then asked again, which
    /// is how "wrap up" quietly became an unbounded extension.
    wrapping_up: bool,
    /// User input for the **running** turn.    ///
    /// Typing while the agent works used to mean waiting: the message sat in the
    /// worker's post-turn queue until the whole turn finished, which for an agentic turn
    /// can be many minutes. Now it is delivered at the next iteration boundary, so
    /// "also check the error path" lands on the next step instead of the next turn.
    /// Boundary delivery rather than mid-flight: the history may only grow where a
    /// complete assistant/tool exchange has closed.
    steer_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    steer_tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// The pid that spawned this worker, when it is a delegated one. Checked at
    /// iteration boundaries so a worker whose parent was killed outright stops instead
    /// of spending on work nobody will read.
    parent_pid: Option<u32>,
    /// Consecutive `final` calls refused because jobs were still running. Bounded: a
    /// refusal loop is the one way this gate could wedge a session, so after a couple
    /// of refusals the loop waits on the foreman's behalf instead.
    final_refusals: u32,
    /// One-shot notice that the window cannot fit the reserve — a config problem, so
    /// repeating it every iteration would just bury the turn.
    zero_budget_warned: bool,
    /// One-shot notice that the *irreducible* pinned head (system + task + memory
    /// index) alone exceeds the budget, so no fold or drop can get under it. Without
    /// this latch, `compact_within_turn` would re-summarize the middle every turn —
    /// ~100 wasted model calls — never shrinking the head that is the actual problem.
    compaction_stuck_warned: bool,
    /// Cached token cost of `tools`, which is fixed once MCP tools are merged in.
    tools_tokens_cache: std::sync::OnceLock<usize>,
    /// Memoized per-message token counts, keyed by a hash of the fields that affect
    /// the count.
    ///
    /// tiktoken is slow enough to matter here: measured at ~570ms for one pass over a
    /// realistic 300-message / 110k-token conversation, and the loop makes several
    /// passes per iteration (budget check, usage report, prompt estimate) for up to
    /// `max_iterations` iterations per turn. Keying on a content hash rather than
    /// tracking mutations means there is no invalidation to get wrong: if a message
    /// changes — `shed_reasoning` drops a `reasoning` field, say — the key changes with
    /// it. Hashing is orders of magnitude cheaper than BPE encoding.
    token_memo: std::cell::RefCell<std::collections::HashMap<u64, usize>>,
    /// The current task statement, remembered so pruning and compaction can protect
    /// it wherever it sits.
    ///
    /// Held as content rather than an index because every prune and fold rebuilds
    /// `messages`, and an index would need fixing up at each one — the kind of
    /// bookkeeping that silently rots. Identified by content, which cannot go stale.
    task: Option<String>,
    /// One-shot latch so the "output limit may be too low" warning fires once.
    output_limit_warned: bool,
    /// Recursion depth for subagents (0 = top-level).
    subagent_depth: usize,
    /// The most recent turn's final message (for the session summary).
    last_final: Option<String>,
    /// Running session token estimates (tiktoken-based; provider-independent).
    tokens_in: u64,
    tokens_out: u64,
    /// USD per 1M input/output tokens (None when the model's pricing is unknown).
    price_in: Option<f64>,
    price_out: Option<f64>,
    /// USD per 1M *cached* input tokens; falls back to `price_in` (no discount).
    price_cached_in: Option<f64>,
    /// Whether the "no cached price, so cost is overstated" notice has been shown.
    warned_no_cache_price: bool,
    /// Provider-reported token totals, preferred over the local estimate when
    /// the stream carries `usage` (billing ground truth; sees cache hits).
    usage_in: u64,
    usage_out: u64,
    usage_cached_in: u64,
    /// True once any response carried provider usage: from then on the estimate
    /// is abandoned entirely (mixing counted and estimated tokens would corrupt
    /// both), and a response *without* usage contributes zero rather than an
    /// estimate.
    usage_reported: bool,
    /// Running estimated session spend in USD (0.0 when pricing is unknown).
    /// This is the agent's OWN spend; subagent spend is tracked separately in
    /// [`Self::subagent_cost_usd`] and added in when reporting to the UI.
    cost_usd: f64,
    /// Spend (USD) and token estimates rolled up from finished subagents, read
    /// from each child's journal as it completes. Kept separate from the agent's
    /// own counters because subagents may run different models (different prices),
    /// so their cost can't be re-derived from the parent's per-token price — it's
    /// summed directly. Each child journals its *combined* total (own + its own
    /// subagents), so this accumulates the whole delegation subtree.
    subagent_cost_usd: f64,
    subagent_tokens_in: u64,
    subagent_tokens_out: u64,
    /// One-shot latch so the 80%-of-budget warning fires only once.
    budget_warned: bool,
    /// The agent's current working plan: (step, status) in order.
    plan: Vec<(String, String)>,
    /// One-shot latch so `SessionStarted` is emitted to the lifecycle log once.
    lifecycle_started: bool,
    /// One-shot latch for the per-session setup step (e.g. `mise install`).
    setup_done: bool,
    /// Loop guard: signature of the last turn's tool calls and how many times in
    /// a row it has repeated. A (sub)agent re-issuing the identical call makes no
    /// progress and burns tokens, so we nudge then abort.
    last_tool_sig: Option<String>,
    /// Digest of the last *executed* tool batch's results, and whether it differed
    /// from the batch before it. The loop guard needs both: an identical call whose
    /// result keeps changing is legitimate polling, not a loop. Only real executions
    /// update these — the guard's own nudge messages must not reset the count, or it
    /// could never escalate to an abort.
    last_obs_sig: Option<String>,
    last_obs_changed: bool,
    tool_repeat: u32,
    /// Companion to `tool_repeat` that counts consecutive turns with the same
    /// *normalized* call **regardless of whether the result changed**. The strict
    /// guard (`tool_repeat`) intentionally exempts a repeated call whose output
    /// keeps changing, because that is legitimate polling. But a *fixating* model
    /// re-runs one inspection with cosmetic churn (which `tool_signature` now folds
    /// away) and gets trivially-different output each time — polling-shaped, yet no
    /// progress. This counter gives that pattern a separate, higher-threshold
    /// backstop so a churner stops well before `max_iterations` without shortening
    /// the rope for genuine polling.
    same_call_repeat: u32,
    /// The previous turn's *raw* (un-normalized) call signature. Distinguishes
    /// byte-identical repetition (polling — the same command re-run for fresh
    /// output) from cosmetic churn (the command edited each turn but folding to the
    /// same normalized signature). Only the latter feeds `same_call_repeat`.
    last_raw_tool_sig: Option<String>,
    /// Plan mode: while on, file-mutating tools (`edit`/`write`) are refused so
    /// the agent proposes a plan and waits for the user to approve (`/go`). Host-
    /// enforced — the agent can't edit during planning even if it tries.
    planning: bool,
    /// Connected MCP servers for this session (host-side). `None` when no servers
    /// are enabled; set via [`AgentLoop::enable_mcp`], which also adds the `mcp`
    /// tool and lists the servers in the system prompt.
    mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    /// (name, builder) for the model to reroute to when the configured one is
    /// permanently unavailable at the provider. See [`Self::with_model_fallback`].
    fallback_model: Option<(String, ModelBuilder)>,
    /// One-shot latch: reroute at most once, so a fallback that is itself missing
    /// can't ping-pong.
    fallback_used: bool,
    messages: Vec<Message>,
    ui: &'a mut dyn AgentUi,
    logger: Option<SessionLogger>,
    /// Container bring-up status lines from the runtime (image pulls/builds,
    /// container + gateway starts), forwarded to the UI as notices — these
    /// phases can take minutes on a cold host and would otherwise be silent.
    runtime_status: tokio::sync::mpsc::UnboundedReceiver<String>,
}

/// A planned subagent delegation, ready to execute. Owns everything it needs so
/// a batch can run concurrently without borrowing the parent loop.
#[derive(Debug)]
struct SubagentPlan {
    exe: std::path::PathBuf,
    root: std::path::PathBuf,
    /// The child's session id (assigned by the parent via `COWBOY_SESSION_ID`), so
    /// the parent advertises it and the UI can watch the child's journal at
    /// `<root>/.cowboy/sessions/<id>/events.jsonl`.
    id: String,
    child_depth: usize,
    /// Full brief sent to the worker (context + task + expected artifact).
    task: String,
    /// The original one-line task, for UI notices.
    display_task: String,
    /// Display label, e.g. `tests/small → cheap`.
    label: String,
    /// The crew-resolved model (routed via `COWBOY_MODEL`); None when no roster.
    model: Option<String>,
    /// Per-task-type temperature override (routed via `COWBOY_TEMPERATURE`).
    temperature: Option<f32>,
    /// Initial iteration grant for the child (effort-scaled), and the host-enforced
    /// ceiling on however many extensions it later earns. `None` when the roster
    /// disabled supervision, in which case the child falls back to
    /// `agent.max_iterations`.
    budget: Option<(u32, u32)>,
    /// The host-side control directory this child reports turn requests through.
    /// `None` when supervision is off or no state directory is available — the child
    /// then simply cannot ask, and is not told it can.
    control_dir: Option<std::path::PathBuf>,
    /// (category, effort, model, fell_back) for the lifecycle event.
    routed: Option<(String, String, String, bool)>,
}

/// A stable hash of the configured `setup` commands, written to the per-worktree
/// marker so changing the commands re-runs them (but unchanged ones are skipped).
fn setup_hash(cmds: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cmds.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Execute one planned subagent: a nested one-shot `cowboy` run in the same worktree.
/// No parent borrow, so many can run concurrently.
///
/// The child brings up its **own** sandbox session — it re-derives the same
/// deterministic session name from the project root, but that name only identifies the
/// project; nothing is shared. An earlier version also passed the parent's name as
/// `COWBOY_CONTAINER_NAME`, a leftover of the Docker runtime that nothing read.
async fn exec_subagent(plan: SubagentPlan) -> String {
    use std::os::unix::process::ExitStatusExt;
    let mut cmd = tokio::process::Command::new(&plan.exe);
    // `--` before the task so a task that happens to start with `-` (e.g.
    // "-v refactoring" or "--wip") is parsed as the positional TASK, not mistaken
    // for a flag by the child's clap parser — which would fail the subagent to start.
    cmd.arg("--")
        .arg(&plan.task)
        .current_dir(&plan.root)
        .env("COWBOY_SUBAGENT_DEPTH", plan.child_depth.to_string())
        // Assign the child its session id so its journal lands at a path the parent
        // already advertised (SubagentStarted { id }) and the UI can watch.
        .env("COWBOY_SESSION_ID", &plan.id)
        .env("COWBOY_PRINT_FINAL_ONLY", "1")
        // Capture (don't inherit) the child's stderr: inheriting would corrupt the
        // parent TUI/console, but discarding it threw away the *reason* a subagent
        // failed — collapsing every failure into a bare "no final answer" that the
        // foreman could only guess about ("resource exhaustion…"). We keep it and
        // surface a tail only when the child actually fails.
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(model) = &plan.model {
        cmd.env("COWBOY_MODEL", model);
    }
    if let Some(t) = plan.temperature {
        cmd.env("COWBOY_TEMPERATURE", t.to_string());
    }
    // The child's iteration budget: a small grant it must report progress to extend,
    // and the ceiling it can never be granted past. Both are set here, host-side, so
    // the worker cannot widen its own budget.
    if let Some((grant, ceiling)) = plan.budget {
        cmd.env(ENV_ITERATION_GRANT, grant.to_string())
            .env(ENV_MAX_TOTAL_ITERATIONS, ceiling.to_string());
    }
    // The channel it asks for more turns on. Host-side, outside the workspace.
    if let Some(dir) = &plan.control_dir {
        cmd.env(crate::agent::jobctl::ENV_JOB_CONTROL_DIR, dir);
    }
    // So the child can notice if we are killed outright and stop rather than keep
    // spending on work nobody will read.
    cmd.env(ENV_PARENT_PID, std::process::id().to_string());
    match cmd.output().await {
        // Clean exit: the final answer is on stdout.
        Ok(o) if o.status.success() => {
            let result = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if result.is_empty() {
                "subagent produced no final answer".to_string()
            } else {
                result
            }
        }
        // The child ran but failed. Report WHY so the foreman (and the user) get a
        // real cause instead of a guess. A signal death is almost always the host
        // OOM-killer; a non-zero exit carries the child's error (a model 429 /
        // RESOURCE_EXHAUSTED, a tool failure) on stderr.
        Ok(o) => {
            let tail = stderr_tail(&String::from_utf8_lossy(&o.stderr));
            let detail = if tail.is_empty() {
                String::new()
            } else {
                format!("\n{tail}")
            };
            if let Some(sig) = o.status.signal() {
                let sigkill = if sig == 9 { " (SIGKILL)" } else { "" };
                format!(
                    "subagent error: killed by signal {sig}{sigkill} — most likely the \
                     host ran out of memory running several subagents at once. Lower \
                     `delegation.max_parallel` (or run fewer subagents per turn), or give \
                     the machine more RAM.{detail}"
                )
            } else {
                let code = o.status.code().unwrap_or(-1);
                format!("subagent error: exited with status {code}{detail}")
            }
        }
        Err(e) => format!("subagent failed to start: {e}"),
    }
}

/// Watch a running child's control directory for turn requests and questions, forwarding
/// each new one to the registry as a [`JobEvent`](crate::agent::jobs::JobEvent).
///
/// Never returns: it is a `select!` branch that runs for as long as the child does.
/// `None` (no control directory) parks forever, which is the right shape for a child
/// that cannot ask.
async fn watch_turn_requests(
    watch: Option<(
        std::path::PathBuf,
        String,
        tokio::sync::mpsc::UnboundedSender<crate::agent::jobs::JobEvent>,
    )>,
) {
    let Some((dir, id, tx)) = watch else {
        std::future::pending::<()>().await;
        return;
    };
    // Requests are numbered from 1 and answered in order, so the watcher only ever
    // looks for the next one — a re-read of an already-forwarded request would
    // re-prompt the foreman about a question it has answered. Questions are a separate
    // sequence for the same reason they are a separate file: they are a different
    // conversation, and a worker can ask one without having asked for turns.
    let mut next_seq = 1u32;
    let mut next_question = 1u32;
    loop {
        tokio::time::sleep(REQUEST_POLL).await;
        let path = dir.join(format!("request-{next_seq}.json"));
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            // A half-written file will parse on the next pass.
            if let Ok(req) = serde_json::from_str::<crate::agent::jobctl::TurnRequest>(&text) {
                let _ = tx.send(crate::agent::jobs::JobEvent::TurnRequest {
                    id: id.clone(),
                    seq: req.seq,
                    report: format!("{}\n\nMeasured: {}", req.report.trim(), req.evidence),
                    requested: req.requested,
                    used: req.used,
                });
                next_seq = req.seq + 1;
            }
        }
        let qpath = dir.join(format!("question-{next_question}.json"));
        if let Ok(text) = tokio::fs::read_to_string(&qpath).await {
            if let Ok(q) = serde_json::from_str::<crate::agent::jobctl::Question>(&text) {
                let _ = tx.send(crate::agent::jobs::JobEvent::Question {
                    id: id.clone(),
                    seq: q.seq,
                    question: q.question,
                    options: q.options,
                });
                next_question = q.seq + 1;
            }
        }
    }
}

/// How often each side polls the control directory. Short enough that a foreman's
/// answer feels immediate, long enough to be free.
const REQUEST_POLL: std::time::Duration = std::time::Duration::from_millis(400);

/// Ceiling on a per-call `shell` timeout. A single command may raise the timeout
/// (a slow test suite is legitimate) but not without bound: an accidental
/// multi-hour value would pin the worker until the turn's cancel or the session
/// ends. One hour is comfortably above any real build/test yet still finite.
const MAX_SHELL_TIMEOUT_SECONDS: u64 = 3600;

/// Spend + token estimates rolled up from one finished subagent.
#[derive(Default, Clone, Copy)]
struct SubagentUsage {
    cost_usd: f64,
    tokens_in: u64,
    tokens_out: u64,
}

/// Read a finished subagent's final cost/token totals from its journal
/// (`<session>/events.jsonl`). The child emits `Cost`/`Tokens` events as it runs
/// (each carrying its *combined* running total, so the last of each is the whole
/// subtree); we take the last value seen. Best-effort: a missing/short journal —
/// e.g. an unpriced model that never emitted `Cost` — just yields zeros, matching
/// the old behavior of not counting it. Called only after the child has exited,
/// so the journal is fully flushed.
fn read_subagent_usage(root: &std::path::Path, id: &str) -> SubagentUsage {
    use cowboy_core::daemonproto::UiEventMsg;
    let path = crate::session::session_dir(root, id).join("events.jsonl");
    let mut usage = SubagentUsage::default();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            match serde_json::from_str::<UiEventMsg>(line) {
                Ok(UiEventMsg::Cost(c)) => usage.cost_usd = c,
                Ok(UiEventMsg::Tokens { input, output }) => {
                    usage.tokens_in = input;
                    usage.tokens_out = output;
                }
                _ => {}
            }
        }
    }
    usage
}

/// Merged user+project model definitions (name → def), best-effort. Maps a
/// routed model name to its provider for the per-provider throttle; a missing or
/// unparseable file just yields fewer entries (callers then key on the model name).
fn load_model_defs(
    root: &std::path::Path,
) -> std::collections::BTreeMap<String, cowboy_core::config::ModelDef> {
    use cowboy_core::config::{ModelsConfig, COWBOY_DIR, MODELS_FILE};
    let mut defs = std::collections::BTreeMap::new();
    if let Some(p) = ModelsConfig::user_path() {
        if let Ok(Some(u)) = ModelsConfig::load_opt(&p) {
            defs.extend(u.models);
        }
    }
    if let Ok(Some(p)) = ModelsConfig::load_opt(&root.join(COWBOY_DIR).join(MODELS_FILE)) {
        defs.extend(p.models);
    }
    defs
}

/// A user-facing notice describing the *true* concurrency of a subagent batch:
/// how many start running now vs. how many queue behind the per-provider cap.
/// Grouping by provider matters because the cap is per provider — three subagents
/// split across three providers all run at once, but three on one provider run
/// `per_provider` at a time.
fn concurrency_notice(
    plans: &[(String, SubagentPlan)],
    per_provider: usize,
    max_parallel: usize,
    defs: &std::collections::BTreeMap<String, cowboy_core::config::ModelDef>,
    foreman: Option<&str>,
) -> String {
    let keys: Vec<String> = plans
        .iter()
        .map(|(_, plan)| provider_key(plan.model.as_deref(), defs, foreman))
        .collect();
    concurrency_notice_from_keys(&keys, per_provider, max_parallel)
}

/// The concurrency-notice math, over already-resolved provider keys (one per
/// planned subagent). Split out so it can be unit-tested without building plans.
fn concurrency_notice_from_keys(
    provider_keys: &[String],
    per_provider: usize,
    max_parallel: usize,
) -> String {
    let total = provider_keys.len();
    let mut per_provider_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for k in provider_keys {
        *per_provider_counts.entry(k.as_str()).or_insert(0) += 1;
    }
    let cap = if per_provider == 0 {
        usize::MAX
    } else {
        per_provider
    };
    let runnable: usize = per_provider_counts
        .values()
        .map(|&n| n.min(cap))
        .sum::<usize>()
        .min(max_parallel.max(1));
    let queued = total.saturating_sub(runnable);
    if queued == 0 {
        format!("↳ running {total} subagents in parallel")
    } else {
        format!(
            "↳ {total} subagents: {runnable} running, {queued} queued \
             (max {per_provider}/provider)"
        )
    }
}

/// The provider a subagent will hit, used to group the per-provider concurrency
/// throttle. A routed model resolves to its `provider`; an unknown model keys on
/// its own name (still groups identical models); a roster-less worker (`None`)
/// runs on the foreman's model, so it keys on the foreman's provider.
fn provider_key(
    model: Option<&str>,
    defs: &std::collections::BTreeMap<String, cowboy_core::config::ModelDef>,
    foreman: Option<&str>,
) -> String {
    match model {
        Some(name) => defs
            .get(name)
            .map(|d| d.provider.clone())
            .unwrap_or_else(|| name.to_string()),
        None => foreman
            .and_then(|f| defs.get(f))
            .map(|d| d.provider.clone())
            .unwrap_or_else(|| "<foreman>".to_string()),
    }
}

/// The last few lines of a child's stderr, bounded, for a failure message: enough
/// to show the cause (a model error, an OOM trace) without dumping a whole log into
/// the foreman's context. Keeps the tail (where the error lands).
fn stderr_tail(stderr: &str) -> String {
    const MAX_LINES: usize = 12;
    const MAX_CHARS: usize = 1500;
    let trimmed = stderr.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() > MAX_LINES {
        lines = lines.split_off(lines.len() - MAX_LINES);
    }
    let tail = lines.join("\n");
    if tail.chars().count() > MAX_CHARS {
        // Keep the end (the actual error), prefixed with an elision marker. Count
        // in chars so we never slice through a multibyte boundary.
        let kept: String = tail
            .chars()
            .rev()
            .take(MAX_CHARS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return format!("…{kept}");
    }
    tail
}

/// Coarsely classify a subagent's result string for the crew history:
/// "error" (failed to start / depth-limited), "empty" (no final answer), else
/// "complete". A heuristic — good enough for usage trends, not a verdict.
fn classify_subagent_result(result: &str) -> &'static str {
    let r = result.trim();
    if r.starts_with("subagent failed to start")
        || r.starts_with("error:")
        || r.starts_with("subagent error")
        || r.starts_with("[incomplete]")
        || r.starts_with("[partial]")
    {
        "error"
    } else if r.is_empty() || r == "subagent produced no final answer" {
        "empty"
    } else {
        "complete"
    }
}

/// Instruction for the context-compaction summary call.
const SUMMARY_SYSTEM: &str = "\
You are compacting an AI coding agent's conversation so it fits the context \
window. Summarize the messages below into a concise but information-dense brief \
that PRESERVES everything needed to continue the task: the user's goals and \
instructions, decisions and their rationale, files created/edited and how, \
commands run and their key results, important facts learned about the codebase, \
and any unresolved problems or next steps. Use terse bullet points; drop \
pleasantries. This summary REPLACES the original messages, so omit nothing load-\
bearing. Output only the summary.";

/// Instruction for the truncation-recovery summary: distill the conclusions a
/// cut-off reasoning trace already reached so the retry can act instead of
/// re-deriving them from scratch.
const REPRIME_SYSTEM: &str = "\
An AI coding agent ran out of output-token budget mid-thought and produced no \
answer or tool call. Below is its (truncated) reasoning. Distill ONLY the \
conclusions it had already reached that bear on the immediate next action: what \
it decided to do, which file/command/tool it settled on and with what arguments, \
and any facts it established. Omit abandoned dead-ends and open questions it \
never resolved. Terse bullet points. Output only the distilled conclusions.";

/// Maximum consecutive summarize-and-reprime retries after a truncation before
/// giving up and reporting `[incomplete]`.
const MAX_REPRIME_ATTEMPTS: u32 = 2;

/// Tokens reserved for the model's response + tool schemas when budgeting.
const RESPONSE_HEADROOM: usize = 4096;

/// How many recent assistant turns keep their `reasoning` for the round-trip.
///
/// Two, because the purpose is continuity across a tool call: the model needs the
/// thinking that led to the call it is now seeing the result of. Older thinking is
/// re-derivable from the messages themselves and is pure prompt weight.
const REASONING_TURNS_KEPT: usize = 2;
/// Maximum subagent nesting depth (prevents runaway recursion).
const MAX_SUBAGENT_DEPTH: usize = 2;

/// Why [`AgentLoop::await_job_news`] stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Woke {
    /// A job reported something, and it was delivered to the conversation.
    News,
    /// The user said something; acting on that comes before waiting.
    Steered,
    /// The bound elapsed (or there was nothing to wait for).
    TimedOut,
    /// The turn was cancelled.
    Cancelled,
}

/// The outcome of a worker asking its foreman for more turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestOutcome {
    /// The budget grew; carry on.
    Continue,
    /// Finish now, with just enough turns to write the answer.
    WrapUp,
    /// The work was abandoned; report what there is and stop.
    Stop,
    /// There is nobody to ask (no control channel).
    Unavailable,
    /// The turn was cancelled while waiting.
    Interrupted,
}

/// What a worker is told when it is out of turns for good.
const WRAP_UP_DIRECTIVE: &str = "[wrap up] You have only enough turns left to report. Stop \
investigating and stop editing. Write up what you established, what you did NOT get to, and \
anything the next worker needs — then call `final` with it. An unreported result is a wasted \
delegation.";

/// Turns handed out when a request goes unanswered, once.
const AUTO_EXTENSION_TURNS: u32 = 10;

/// Whether a free-text answer means "keep going".
///
/// Deliberately narrow, and silence is **not** consent: an empty answer is what
/// `ask_user` returns when nobody can answer (a piped run, no attached client), so
/// treating it as yes would make a non-interactive session extend itself forever. Only an
/// explicit affirmative counts; anything else ends the turn, which is recoverable.
fn is_affirmative(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "yeah" | "yep" | "ok" | "okay" | "sure" | "continue" | "keep going" | "go"
    )
}

/// Hard cap on how many times one worker may ask. The ceiling already bounds total
/// turns; this bounds the *ping-pong*, so a worker and a foreman cannot spend a
/// session negotiating.
const MAX_TURN_REQUESTS: u32 = 6;

/// Hard cap on how many times the *user* may be asked to extend one message's budget.
///
/// Generous, because a human saying yes is real consent rather than a guess — but not
/// unbounded: a user who holds down Enter should not be able to turn one message into an
/// indefinite run, and after this many extensions the honest move is to end the turn and
/// let them send a fresh message (which starts a clean budget, with the conversation
/// intact either way).
const MAX_USER_EXTENSIONS: u32 = 10;

/// How long `wait` parks by default, and the hard cap on what a model can ask for. A
/// model that asks to wait an hour has misjudged; the ceiling keeps a mistake cheap.
const WAIT_DEFAULT_SECONDS: u64 = 300;
const WAIT_MAX_SECONDS: u64 = 1800;

/// Maximum consecutive `final` calls refused because subagents are still running.
/// After this the loop waits for them itself: arguing with the model forever is worse
/// than a bounded wait.
const MAX_FINAL_REFUSALS: u32 = 2;

/// How long that fallback wait lasts before the turn finishes regardless.
const FINAL_AUTO_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Env: the pid of the process that spawned this worker.
///
/// Session-scoped jobs deliberately outlive individual turns, so a worker is only
/// reaped by its parent asking it to stop — and a parent that is `kill -9`ed never
/// asks. The child therefore checks for itself.
///
/// `PR_SET_PDEATHSIG` would be the kernel's answer to this, and is deliberately not
/// used: it fires when the *spawning thread* exits, not the process, which on a
/// multi-thread tokio runtime means a worker can be killed the moment the pool
/// retires whichever thread happened to spawn it. A liveness check at the worker's own
/// iteration boundaries is slower to notice but cannot misfire.
const ENV_PARENT_PID: &str = "COWBOY_PARENT_PID";

/// Env: a delegated worker's initial iteration grant, set by the parent that
/// spawned it. Absent for the foreman, which uses `agent.max_iterations`.
const ENV_ITERATION_GRANT: &str = "COWBOY_ITERATION_GRANT";
/// Env: the host-enforced ceiling on a delegated worker's total iterations. The
/// foreman may grant extensions; it may not raise this.
const ENV_MAX_TOTAL_ITERATIONS: &str = "COWBOY_MAX_TOTAL_ITERATIONS";

impl<'a> AgentLoop<'a> {
    pub fn new(
        model: Box<dyn ModelClient>,
        runtime: impl Sandbox + 'static,
        behavior: AgentBehavior,
        context_window: usize,
        cancel: CancellationToken,
        ui: &'a mut dyn AgentUi,
    ) -> Self {
        // Boxed here rather than by callers so the many construction sites (and
        // tests) stay unchanged when the sandbox backend does.
        let mut runtime: Box<dyn Sandbox> = Box::new(runtime);
        let runtime_status = runtime.status_channel();
        // Crew mode (roster + delegation enabled) gates the foreman guidance and
        // the `subagent` tool — and so does depth: a worker already at the limit
        // is offered neither, rather than being told to delegate and refused when
        // it tries. Loaded once here; `dispatch_subagents` re-reads the roster per
        // turn so a mid-session `cowboy crew` edit still takes effect on routing.
        let crew_cfg = cowboy_core::crew::load().ok().flatten();
        let crew_on = crew_cfg.as_ref().is_some_and(|c| c.enabled());
        let subagent_depth = std::env::var("COWBOY_SUBAGENT_DEPTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let can_delegate = delegation_available(crew_on, subagent_depth, crew_cfg.as_ref());
        // Computed before the struct literal, which moves `behavior`.
        let budget = IterationBudget::from_env(behavior.max_iterations);
        // A worker may ask for more turns only if it has a grant *and* a live channel
        // to ask on. With a grant but no channel it would block for the request
        // timeout and be answered by the fallback — worse than not asking.
        let control = crate::agent::jobctl::ControlDir::from_env();
        let can_request_turns = budget.supervised && control.is_some();
        let system = system_prompt(
            can_delegate,
            subagent_depth,
            can_request_turns,
            crew_cfg.as_ref(),
        );
        let tools = tool_surface(can_delegate, can_request_turns);
        let stall_window = crew_cfg
            .as_ref()
            .map(|c| c.delegation.stall_window)
            .unwrap_or_else(|| cowboy_core::crew::Delegation::default().stall_window);
        let delegation = crew_cfg
            .as_ref()
            .map(|c| c.delegation.clone())
            .unwrap_or_default();
        let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            model,
            summarizer: None,
            runtime,
            tools,
            behavior,
            cancel,
            context_window,
            reprime_attempts: 0,
            minimize_reasoning_next_turn: false,
            reasoning_shed_notified: false,
            budget,
            grant_stage_seen: GrantStage::Fine,
            progress: ProgressTracker::default(),
            verification: Verification::default(),
            seen_files: SeenFiles::default(),
            processes: std::collections::BTreeMap::new(),
            stall_window,
            stall_count: 0,
            jobs: crate::agent::jobs::JobRegistry::new(
                delegation.max_parallel_per_provider as usize,
            ),
            job_tx,
            job_rx,
            fanout_sem: std::sync::Arc::new(tokio::sync::Semaphore::new(
                delegation.max_parallel.max(1) as usize,
            )),
            job_stopper: crate::agent::jobs::JobStopper::default(),
            control: can_request_turns.then_some(control).flatten(),
            turn_requests: 0,
            user_extensions: 0,
            auto_extensions: 0,
            request_timeout: std::time::Duration::from_secs(
                delegation.request_timeout_seconds.max(5),
            ),
            wrapping_up: false,
            steer_rx,
            steer_tx,
            parent_pid: std::env::var(ENV_PARENT_PID)
                .ok()
                .and_then(|v| v.parse().ok()),
            final_refusals: 0,
            zero_budget_warned: false,
            compaction_stuck_warned: false,
            tools_tokens_cache: std::sync::OnceLock::new(),
            token_memo: std::cell::RefCell::new(std::collections::HashMap::new()),
            task: None,
            output_limit_warned: false,
            subagent_depth,
            last_final: None,
            tokens_in: 0,
            tokens_out: 0,
            price_in: None,
            price_out: None,
            price_cached_in: None,
            warned_no_cache_price: false,
            usage_in: 0,
            usage_out: 0,
            usage_cached_in: 0,
            usage_reported: false,
            cost_usd: 0.0,
            subagent_cost_usd: 0.0,
            subagent_tokens_in: 0,
            subagent_tokens_out: 0,
            budget_warned: false,
            plan: Vec::new(),
            lifecycle_started: false,
            setup_done: false,
            last_tool_sig: None,
            last_obs_sig: None,
            last_obs_changed: false,
            tool_repeat: 0,
            same_call_repeat: 0,
            last_raw_tool_sig: None,
            planning: false,
            mcp: None,
            fallback_model: None,
            fallback_used: false,
            messages: vec![Message::system(system)],
            ui,
            logger: None,
            runtime_status,
        }
    }

    /// Connect this session to the configured MCP servers: list them (name +
    /// purpose) in the system prompt so the agent knows what's available, and add
    /// the `mcp` discovery/call tool. No-op if no servers are enabled.
    pub fn enable_mcp(&mut self, manager: std::sync::Arc<crate::mcp::McpManager>) {
        let servers = manager.connected_servers();
        if servers.is_empty() {
            return;
        }
        let mut block = String::from(
            "\n\n## Connected MCP servers\n\
             You have access to these external MCP servers (host-managed integrations). \
             Use the `mcp` tool to discover their tools (`list_tools`) and call them (`call`); \
             discover a server's tools before calling them:\n",
        );
        for (name, desc) in &servers {
            if desc.is_empty() {
                block.push_str(&format!("- {name}\n"));
            } else {
                block.push_str(&format!("- {name}: {desc}\n"));
            }
        }
        if let Some(Message { content, .. }) = self.messages.first_mut() {
            content.push_str(&block);
        }
        self.tools.push(tools::mcp_definition());
        self.mcp = Some(manager);
    }

    /// Accumulate per-call token usage (prompt sent + completion received)
    /// and report the running session total to the UI. Provider-reported usage
    /// is the billing ground truth — it sees prompt-cache hits — so once any
    /// response carries it we account on it exclusively; until then we fall
    /// back to the local tokenizer estimate (provider-independent, roughly
    /// tracks billing, but blind to caching).
    fn account_tokens(&mut self, prompt_est: u64, response: &ChatResponse) {
        if let Some(u) = response.usage {
            self.usage_reported = true;
            self.usage_in += u.prompt_tokens;
            self.usage_out += u.completion_tokens;
            self.usage_cached_in += u.cached_prompt_tokens.min(u.prompt_tokens);
            self.tokens_in += u.prompt_tokens;
            self.tokens_out += u.completion_tokens;
        } else if self.usage_reported {
            // Earlier calls reported usage, so the totals are counted, not
            // estimated — adding an estimate now would corrupt both. A missing
            // usage chunk contributes zero (slight undercount) instead.
        } else {
            self.tokens_in += prompt_est;
            let mut out =
                cowboy_core::tokens::count(response.content.as_deref().unwrap_or_default()) as u64;
            // Reasoning is billed as output and can dwarf the visible answer on the
            // reasoning models this targets; omitting it made spend/budget read far
            // below the truth.
            out += cowboy_core::tokens::count(response.reasoning.as_deref().unwrap_or_default())
                as u64;
            for tc in &response.tool_calls {
                out += (cowboy_core::tokens::count(&tc.arguments)
                    + cowboy_core::tokens::count(&tc.name)) as u64;
            }
            self.tokens_out += out;
        }
        self.report_usage();
    }

    /// Recompute the agent's own cost from its tokens, then report the SESSION
    /// total — own usage plus everything rolled up from subagents — to the UI.
    /// The displayed token and cost figures therefore include delegated work,
    /// which previously vanished from the total (subagents run as separate
    /// processes that only journal into their own session). Tokens are always
    /// reported; cost is reported when we have either local pricing or a non-zero
    /// subagent cost (a subagent may be priced even when this agent isn't).
    fn report_usage(&mut self) {
        if let (Some(pi), Some(po)) = (self.price_in, self.price_out) {
            self.cost_usd = if self.usage_reported {
                // Counted tokens: bill cache hits at the cache price (default:
                // full input price), the rest of the prompt at the input price.
                let pc = self.price_cached_in.unwrap_or(pi);
                let uncached = self.usage_in.saturating_sub(self.usage_cached_in);
                (uncached as f64 / 1e6) * pi
                    + (self.usage_cached_in as f64 / 1e6) * pc
                    + (self.usage_out as f64 / 1e6) * po
            } else {
                (self.tokens_in as f64 / 1e6) * pi + (self.tokens_out as f64 / 1e6) * po
            };
            // Say so, once, when the number on screen is knowably too high.
            //
            // With no cache price the fallback is the full input price, which is the
            // largest defensible value — it never understates spend, which is the right
            // bias for a cost display. But for an agent workload it is wrong by a lot:
            // nearly every request re-sends a cached prefix, and measured discounts run
            // 3%–19% of the input price. One real session read $18.70 against a $2.50
            // bill for exactly this reason, and nothing on screen hinted at why.
            //
            // A warning rather than an assumed discount, because the discount varies far
            // too much between models to guess: guessing would trade a knowable
            // overstatement for an unknowable error in either direction.
            if self.price_cached_in.is_none()
                && self.usage_cached_in > 0
                && !self.warned_no_cache_price
            {
                self.warned_no_cache_price = true;
                let pct = (self.usage_cached_in as f64 / self.usage_in.max(1) as f64) * 100.0;
                self.ui.notice(&format!(
                    "cost is overstated: {pct:.0}% of prompt tokens are cache reads, billed at \
                     the full input rate because this model has no `cached_input_cost_per_mtok`. \
                     Add it (see your provider's cached-input price) for an accurate figure."
                ));
            }
        }
        self.ui.tokens(
            self.tokens_in + self.subagent_tokens_in,
            self.tokens_out + self.subagent_tokens_out,
        );
        if self.price_in.is_some() || self.subagent_cost_usd > 0.0 {
            self.ui.cost(self.cost_usd + self.subagent_cost_usd);
        }
    }

    /// Replace the working plan, surface it to the UI, and echo it back to the
    /// model as the tool observation. Statuses are normalized to a known set.
    fn run_plan(&mut self, args: PlanArgs) -> String {
        let prev = std::mem::take(&mut self.plan);
        self.plan = args
            .steps
            .into_iter()
            .map(|s| {
                let status = match s.status.as_deref().map(str::trim).unwrap_or("pending") {
                    "in_progress" | "in progress" | "doing" | "active" => "in_progress",
                    "done" | "complete" | "completed" | "finished" => "done",
                    _ => "pending",
                };
                (s.step, status.to_string())
            })
            .collect();
        // Emit lifecycle events for steps that newly entered in_progress/done.
        use cowboy_core::lifecycle::LifecycleEvent;
        let was = |step: &str| {
            prev.iter()
                .find(|(s, _)| s == step)
                .map(|(_, st)| st.as_str())
        };
        for (step, status) in &self.plan {
            let before = was(step);
            match status.as_str() {
                "in_progress" if before != Some("in_progress") => {
                    self.emit_lifecycle(LifecycleEvent::PlanStepStarted { step: step.clone() });
                }
                "done" if before != Some("done") => {
                    self.emit_lifecycle(LifecycleEvent::PlanStepCompleted { step: step.clone() });
                }
                _ => {}
            }
        }
        self.ui.plan(&self.plan);
        let done = self.plan.iter().filter(|(_, s)| s == "done").count();
        let rendered = render_plan(&self.plan);
        format!(
            "Plan updated ({done}/{} done):\n{rendered}",
            self.plan.len()
        )
    }

    /// Append a semantic lifecycle event to the session log (best-effort, no-op
    /// without a logger). These drive Ranch coordination + the message bus.
    fn emit_lifecycle(&self, event: cowboy_core::lifecycle::LifecycleEvent) {
        if let Some(l) = &self.logger {
            cowboy_core::lifecycle::append_in(l.dir(), l.id(), event, now_ms());
        }
    }

    /// Hard stop reason if a configured budget has been reached, else `None`.
    /// Budgets are for the whole session, so subagent usage counts too.
    fn budget_reached(&self) -> Option<String> {
        let b = &self.behavior;
        let used =
            self.tokens_in + self.tokens_out + self.subagent_tokens_in + self.subagent_tokens_out;
        if b.token_budget > 0 && used >= b.token_budget {
            return Some(format!(
                "token budget reached ({used} tokens ≥ {}); stopping",
                b.token_budget
            ));
        }
        let spent = self.cost_usd + self.subagent_cost_usd;
        if b.cost_budget_usd > 0.0 && spent >= b.cost_budget_usd {
            return Some(format!(
                "cost budget reached (${:.2} ≥ ${:.2}); stopping",
                spent, b.cost_budget_usd
            ));
        }
        None
    }

    /// Emit a one-time notice when usage crosses 80% of a configured budget.
    fn maybe_warn_budget(&mut self) {
        if self.budget_warned {
            return;
        }
        let b = &self.behavior;
        let used =
            self.tokens_in + self.tokens_out + self.subagent_tokens_in + self.subagent_tokens_out;
        let spent = self.cost_usd + self.subagent_cost_usd;
        let warn = if b.token_budget > 0 && used as f64 >= 0.8 * b.token_budget as f64 {
            Some(format!(
                "approaching token budget ({used}/{} tokens)",
                b.token_budget
            ))
        } else if b.cost_budget_usd > 0.0 && spent >= 0.8 * b.cost_budget_usd {
            Some(format!(
                "approaching cost budget (${:.2}/${:.2})",
                spent, b.cost_budget_usd
            ))
        } else {
            None
        };
        if let Some(w) = warn {
            self.ui.notice(&w);
            self.budget_warned = true;
        }
    }

    /// Approximate token count of a message (content + reasoning + tool calls).
    ///
    /// `reasoning` counts because it is **sent back** on every subsequent request
    /// (`inject_reasoning_content`) to keep agentic reasoning models on plan. Not
    /// counting it made `fit_context` believe a prompt fit when the real request
    /// overflowed the context window.
    fn message_tokens(m: &Message) -> usize {
        let mut n = cowboy_core::tokens::count(&m.content) + 4;
        n += m
            .reasoning
            .as_deref()
            .map(cowboy_core::tokens::count)
            .unwrap_or(0);
        for tc in &m.tool_calls {
            n += cowboy_core::tokens::count(&tc.arguments)
                + cowboy_core::tokens::count(&tc.name)
                + 4;
        }
        n
    }

    /// Approximate token count of a message, memoized. See [`Self::token_memo`].
    fn tokens_of(&self, m: &Message) -> usize {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // Exactly the fields `message_tokens` reads.
        m.content.hash(&mut h);
        m.reasoning.hash(&mut h);
        for tc in &m.tool_calls {
            tc.name.hash(&mut h);
            tc.arguments.hash(&mut h);
        }
        let key = h.finish();
        if let Some(n) = self.token_memo.borrow().get(&key) {
            return *n;
        }
        let n = Self::message_tokens(m);
        let mut memo = self.token_memo.borrow_mut();
        // Folds and prunes drop messages without dropping their keys. Clearing wholesale
        // beats tracking liveness for a cache this cheap to refill.
        if memo.len() > 4096 {
            memo.clear();
        }
        memo.insert(key, n);
        n
    }

    /// Total estimated tokens of the current conversation.
    fn total_tokens(&self) -> usize {
        self.messages.iter().map(|m| self.tokens_of(m)).sum()
    }

    /// Drop `reasoning` from all but the most recent assistant turns.
    ///
    /// Reasoning is round-tripped to the provider on **every** request
    /// (`inject_reasoning_content`) so an agentic reasoning model keeps its plan
    /// across tool-use turns. Nothing ever shed it, so it accumulated for the life of
    /// the session and was re-sent in full each call: measured on a reasoning model at
    /// ~3k reasoning tokens per turn, turn 31 was carrying ~90k tokens of old thinking
    /// — far more than the actual work, and the dominant reason a long session starts
    /// truncating and compacting.
    ///
    /// Keeping the plan across tool calls needs the last turn or two, not all of them,
    /// so this is a large reduction that costs no extra model call. It runs every
    /// iteration rather than only under pressure: shedding early keeps the
    /// conversation from ever reaching the point where compaction (which does cost a
    /// call) is needed.
    ///
    /// Only the in-memory copy is trimmed. `transcript.jsonl` was written when each
    /// message was recorded, so replay and `--resume` still have the full reasoning.
    fn shed_reasoning(&mut self) -> usize {
        let mut kept = 0usize;
        let mut freed = 0usize;
        for m in self.messages.iter_mut().rev() {
            let Some(r) = m.reasoning.as_deref() else {
                continue;
            };
            if kept < REASONING_TURNS_KEPT {
                kept += 1;
                continue;
            }
            freed += cowboy_core::tokens::count(r);
            m.reasoning = None;
        }
        freed
    }

    /// Tokens the tool schemas add to every request.
    ///
    /// Computed once and cached: the set is fixed after construction (MCP tools are
    /// merged in by `enable_mcp` before the first turn), and re-tokenizing ~16 JSON
    /// schemas on every iteration would be pure waste.
    ///
    /// This is not a rounding error. Measured on the default tool surface: 16
    /// definitions, ~15.8 KB of JSON, **~3.6k tokens** — sent on every single call and
    /// previously invisible to the budget, which counted only `messages`.
    fn tools_tokens(&self) -> usize {
        *self.tools_tokens_cache.get_or_init(|| {
            self.tools
                .iter()
                .map(|d| {
                    cowboy_core::tokens::count(&d.name)
                        + cowboy_core::tokens::count(&d.description)
                        + serde_json::to_string(&d.parameters)
                            .map(|s| cowboy_core::tokens::count(&s))
                            .unwrap_or(0)
                        + 4
                })
                .sum()
        })
    }

    /// What the conversation may occupy, after reserving room for everything else in
    /// the request.
    ///
    /// The reserve is the response budget **plus** the tool schemas, plus a small
    /// floor. It used to be `max(max_output_tokens, RESPONSE_HEADROOM)`, whose comment
    /// claimed the floor "also covers tool-schema overhead" — but `max` means the
    /// floor is superseded the moment `max_output_tokens` exceeds it, which is the
    /// normal case. So the schemas had no allowance at all: with `max_tokens: 32768`
    /// against a 200k window the slack absorbed them, while a model with
    /// `max_tokens: 2048` in an 8k window computed a 4096-token budget for a request
    /// that also carried ~3.6k of schemas — and overflowed at the provider.
    fn context_budget(&self) -> usize {
        let reserve = self.model.max_output_tokens() + self.tools_tokens() + RESPONSE_HEADROOM;
        self.context_window.saturating_sub(reserve)
    }

    /// A snapshot of what the live prompt costs and where the weight sits.
    ///
    /// `top` groups messages by what produced them rather than listing them, because
    /// the actionable question is "which *kind* of thing is filling my window" — tool
    /// output, the model's own reasoning, the task and system prompt, or the
    /// conversation itself. The tool schemas are included because they are sent on
    /// every request and were the least visible term of all.
    fn context_usage(&self) -> ContextUsage {
        let mut reasoning = 0u64;
        let mut tool_results = 0u64;
        let mut assistant = 0u64;
        let mut user = 0u64;
        let mut system = 0u64;
        for m in &self.messages {
            let total = self.tokens_of(m) as u64;
            let r = m
                .reasoning
                .as_deref()
                .map(cowboy_core::tokens::count)
                .unwrap_or(0) as u64;
            reasoning += r;
            let rest = total.saturating_sub(r);
            match m.role {
                Role::Tool => tool_results += rest,
                Role::Assistant => assistant += rest,
                Role::User => user += rest,
                Role::System => system += rest,
            }
        }
        let mut top: Vec<(String, u64)> = vec![
            ("tool results".to_string(), tool_results),
            ("assistant messages".to_string(), assistant),
            ("model reasoning".to_string(), reasoning),
            ("your messages".to_string(), user),
            ("system + summaries".to_string(), system),
            ("tool schemas".to_string(), self.tools_tokens() as u64),
        ];
        top.retain(|(_, n)| *n > 0);
        top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        top.truncate(6);
        ContextUsage {
            used: self.total_tokens() as u64,
            budget: self.context_budget() as u64,
            window: self.context_window as u64,
            reserve: (self.context_window - self.context_budget()) as u64,
            top,
        }
    }

    /// Keep the conversation within the context window. When it overflows, fold
    /// the oldest whole turns into a single model-generated summary message
    /// rather than dropping them, so earlier decisions, edits, and facts survive.
    /// Compaction happens at user-turn boundaries (turn starts) so a tool result
    /// is never orphaned. Falls back to dropping if a summary can't be made.
    async fn fit_context(&mut self) {
        let budget = self.context_budget();
        if budget == 0 {
            // The window cannot hold the reserve, so there is no room for a
            // conversation at all and nothing to trim. Silence here meant the request
            // went out anyway and failed at the provider with a context-length error
            // that names none of this. Say it once, with the numbers.
            if !self.zero_budget_warned {
                self.zero_budget_warned = true;
                self.ui.notice(&format!(
                    "context_window ({}) is too small for this model's max_tokens ({}) \
                     plus {} tokens of tool schemas — raise context_window or lower \
                     max_tokens in models.yaml",
                    self.context_window,
                    self.model.max_output_tokens(),
                    self.tools_tokens()
                ));
            }
            return;
        }
        if self.total_tokens() <= budget {
            return;
        }

        // User messages mark turn starts. Keep the most recent whole turns that
        // fit in part of the budget; summarize everything before them.
        let user_idxs: Vec<usize> = (1..self.messages.len())
            .filter(|&i| self.messages[i].role == Role::User)
            .collect();
        let tail_budget = (budget * 6 / 10).max(1);
        let mut keep_from = match user_idxs.last() {
            Some(&i) => i,
            None => {
                self.drop_oldest(budget);
                return;
            }
        };
        // Suffix sums, so walking back over the turn boundaries is one pass rather than
        // a fresh sum per boundary (which was quadratic in the message count).
        let mut suffix = vec![0usize; self.messages.len() + 1];
        for i in (0..self.messages.len()).rev() {
            suffix[i] = suffix[i + 1] + self.tokens_of(&self.messages[i]);
        }
        for &idx in user_idxs.iter().rev() {
            if suffix[idx] <= tail_budget {
                keep_from = idx;
            } else {
                break;
            }
        }
        // The tail from the last user message alone doesn't fit — i.e. ONE turn has
        // outgrown the budget. This is the common case, not an exotic one: a
        // one-shot `cowboy "do X"` (and every subagent) has a single user message,
        // so there are no earlier turns to fold and the old code fell straight to
        // `drop_oldest`, whose first victim was the task statement itself. Compact
        // *inside* the turn instead, keeping the task pinned.
        if keep_from <= self.pinned() {
            self.compact_within_turn(budget, tail_budget).await;
            return;
        }

        let old: Vec<Message> = self.messages[1..keep_from].to_vec();
        let folded = old.len();
        // A resumed session has the task somewhere in this span rather than at the
        // head; carry it through verbatim instead of summarizing it away.
        let task = self.task_in(1..keep_from);
        let summary = match self.summarize(&old).await {
            Ok(s) if !s.trim().is_empty() => s,
            _ => {
                self.drop_oldest(budget);
                return;
            }
        };
        let mut rebuilt = Vec::with_capacity(self.messages.len() - folded + 2);
        rebuilt.push(self.messages[0].clone());
        if let Some(task) = task {
            rebuilt.push(task);
        }
        rebuilt.push(Message::system(format!(
            "[Summary of earlier conversation, compacted to save context]\n{summary}"
        )));
        rebuilt.extend_from_slice(&self.messages[keep_from..]);
        self.messages = rebuilt;
        self.ui.notice(&format!(
            "compacted {folded} earlier messages into a summary"
        ));
    }

    /// Leading messages that pruning and compaction must never remove: the system
    /// prompt, plus the task statement (the first user message) when present. An
    /// agent that loses its task keeps working with no idea what it's working on —
    /// the classic goal-drift failure of a long autonomous run.
    fn pinned(&self) -> usize {
        let Some(first) = self.messages.get(1) else {
            return 1;
        };
        if first.role != Role::User {
            return 1;
        }
        match self.task.as_deref() {
            // The task is known: pin it only if it really is at the head.
            Some(task) => usize::from(first.content == task) + 1,
            // No task recorded (a caller that seeded `messages` directly): fall back
            // to the old positional guess, so this is never worse than before.
            None => 2,
        }
    }

    /// The task statement, if a fold over `span` would swallow it.
    ///
    /// After `--resume` the task is **not** at index 1: `with_history` inserts the
    /// previous session's transcript there, so `messages[1]` is that session's oldest
    /// user message. `pinned()` used to return 2 for any user message in that slot,
    /// which meant a resumed session pinned stale history and left the real task
    /// protected only by being recent — until a fold reached it. Whatever survives, the
    /// task must: an agent that loses it keeps working with no idea what it is working
    /// on, which is the goal-drift failure the pin exists to prevent.
    fn task_in(&self, span: std::ops::Range<usize>) -> Option<Message> {
        let task = self.task.as_deref()?;
        self.messages
            .get(span)?
            .iter()
            .find(|m| m.role == Role::User && m.content == task)
            .cloned()
    }

    /// Fold the middle of an over-long *single turn* into a summary, keeping the
    /// pinned head (system + task) and the most recent messages. Cuts only at a
    /// boundary that isn't a tool result, so an assistant's tool calls are never
    /// separated from their answers (which providers reject).
    async fn compact_within_turn(&mut self, budget: usize, tail_budget: usize) {
        let pin = self.pinned();
        // If the pinned head alone (system + task + any carried memory index) already
        // meets or exceeds the budget, no fold or drop can get the conversation under
        // it — the irreducible head IS the overflow. Summarizing the middle would burn
        // a model call and change nothing, and `fit_context` runs every turn, so
        // without stopping here that is ~100 wasted summarizations. Warn once and bail;
        // the request goes out over budget and the provider decides (a turn that can't
        // fit its own head is a config problem — context_window too small — not
        // something compaction can solve).
        let head_tokens: usize = self.messages[..pin].iter().map(|m| self.tokens_of(m)).sum();
        if head_tokens >= budget {
            if !self.compaction_stuck_warned {
                self.compaction_stuck_warned = true;
                self.ui.notice(&format!(
                    "the pinned context (system prompt + task) is {head_tokens} tokens, at or \
                     over the {budget}-token budget — cannot compact further; raise \
                     context_window or shorten the task/system prompt"
                ));
            }
            return;
        }
        // The earliest safe cut whose tail fits — keeps as much recent context as
        // the budget allows.
        let mut suffix = vec![0usize; self.messages.len() + 1];
        for i in (0..self.messages.len()).rev() {
            suffix[i] = suffix[i + 1] + self.tokens_of(&self.messages[i]);
        }
        let cut = (pin..self.messages.len())
            .filter(|&i| self.messages[i].role != Role::Tool)
            .find(|&i| suffix[i] <= tail_budget);
        let Some(cut) = cut.filter(|&c| c > pin) else {
            // Nothing foldable (or no safe boundary): fall back to dropping, which
            // still preserves the pinned head.
            self.drop_oldest(budget);
            return;
        };

        let old: Vec<Message> = self.messages[pin..cut].to_vec();
        let folded = old.len();
        let task = self.task_in(pin..cut);
        let Ok(summary) = self.summarize(&old).await else {
            self.drop_oldest(budget);
            return;
        };
        if summary.trim().is_empty() {
            self.drop_oldest(budget);
            return;
        }
        let mut rebuilt = Vec::with_capacity(self.messages.len() - folded + 2);
        rebuilt.extend_from_slice(&self.messages[..pin]);
        if let Some(task) = task {
            rebuilt.push(task);
        }
        rebuilt.push(Message::system(format!(
            "[Summary of earlier work on this task, compacted to save context]\n{summary}"
        )));
        rebuilt.extend_from_slice(&self.messages[cut..]);
        self.messages = rebuilt;
        self.ui.notice(&format!(
            "compacted {folded} earlier messages from this turn into a summary"
        ));
    }

    /// Run a one-shot summarization on the dedicated summarizer model, falling
    /// back to the main model when none is configured. No tools, no streaming.
    async fn run_summary(&self, system: &str, body: String) -> Result<String> {
        let msgs = vec![Message::system(system), Message::user(body)];
        let client = self.summarizer.as_deref().unwrap_or(self.model.as_ref());
        // Minimal reasoning where the backend allows it. Summarizing is mechanical, so
        // extended thinking here buys nothing — and when this falls back to the main
        // model it is the very model that just truncated while thinking, which made the
        // salvage come back empty exactly when it was needed.
        let low = client.with_minimal_reasoning();
        let client = low.as_deref().unwrap_or(client);
        let resp = client.chat(&msgs, &[], None).await?;
        let summary = resp.content.unwrap_or_default();
        // Cap it. The whole point of a fold is that the result is smaller than what it
        // replaced, and nothing enforced that: `fit_context` leaves 40% of the budget
        // for the system prompt, the task and this summary, but a model is free to
        // return an essay. A summary that overflows its own allowance turns one
        // compaction into a loop of them.
        Ok(cowboy_core::tokens::truncate_to_tokens(
            &summary,
            self.summary_token_cap(),
        ))
    }

    /// How many tokens a compaction summary may occupy.
    ///
    /// A fraction of the tail allowance rather than a constant, so it scales with the
    /// window instead of being generous on a small model and stingy on a large one.
    fn summary_token_cap(&self) -> usize {
        (self.context_budget() / 10).clamp(256, 8192)
    }

    /// One-shot warning that the model's configured output-token limit may be
    /// too low: it's spending the whole budget on reasoning before it can answer.
    fn warn_output_limit(&mut self) {
        if self.output_limit_warned {
            return;
        }
        self.output_limit_warned = true;
        self.ui.notice(&format!(
            "model exhausted its output-token budget while reasoning (max_tokens ≈ {}); \
             it may be set too low — raise it in models.yaml or lower the reasoning effort",
            self.model.max_output_tokens()
        ));
    }

    /// Distill a truncated turn's reasoning into a directive that re-primes the
    /// model to act — or, when there is nothing to distill, still tell it to act.
    ///
    /// This used to return `None` in either of those cases and the caller gave up with
    /// `[incomplete]`, which is the stall people kept reporting. Both cases are common:
    /// plenty of providers bill reasoning tokens without ever returning the text, and
    /// the distillation runs on the same model that just proved it will spend its whole
    /// budget thinking, so the summary comes back empty too. Neither is a reason to
    /// abandon the turn — the model has a full transcript and can simply be asked to
    /// finish. Salvaged reasoning makes the retry better, not possible.
    async fn reprime_directive(&self, response: &ChatResponse) -> String {
        const ACT_NOW: &str = "Do NOT reason further. Immediately output your final \
                               answer or the next tool call.";
        let salvaged = match response
            .reasoning
            .as_deref()
            .filter(|r| !r.trim().is_empty())
        {
            Some(reasoning) => self
                .run_summary(REPRIME_SYSTEM, reasoning.to_string())
                .await
                .ok()
                .filter(|s| !s.trim().is_empty()),
            None => None,
        };
        match salvaged {
            Some(summary) => format!(
                "Your previous attempt ran out of thinking budget before you answered. \
                 Here is what you had already concluded:\n\n{summary}\n\n{ACT_NOW}"
            ),
            None => format!(
                "Your previous attempt ran out of its output-token budget while thinking \
                 and produced nothing. Do not start over and do not re-derive your \
                 reasoning: work from the conversation above. If you cannot finish the \
                 whole task in one answer, take the single next concrete step. {ACT_NOW}"
            ),
        }
    }

    /// Ask the model to summarize a span of prior messages into a dense brief.
    async fn summarize(&self, old: &[Message]) -> Result<String> {
        self.run_summary(
            SUMMARY_SYSTEM,
            format!("{}\n\n---\nWrite the summary now.", render_transcript(old)),
        )
        .await
    }

    /// Last-resort pruning: drop the oldest messages, never the pinned head (the
    /// system prompt **and the task**), skipping orphaned tool results, until
    /// within budget.
    /// Last resort when a summary can't be made: drop the oldest history until the
    /// conversation fits.
    ///
    /// Rebuilds rather than removing repeatedly. The old loop called `total_tokens()`
    /// once per removed message, re-tokenizing the whole conversation each time — O(n²)
    /// on the path taken when the model is already struggling. This walks backwards
    /// once, keeping the newest messages that fit.
    ///
    /// The system prompt and the task statement always survive, wherever the task sits
    /// (after `--resume` it is not in the head). A tool result is only kept if the
    /// assistant turn that called it is kept too, since providers reject a result with
    /// no matching call.
    fn drop_oldest(&mut self, budget: usize) {
        let pin = self.pinned();
        let head: Vec<Message> = self.messages[..pin].to_vec();
        let task = self.task_in(pin..self.messages.len());
        let mut used: usize = head.iter().map(|m| self.tokens_of(m)).sum();
        used += task.as_ref().map(|m| self.tokens_of(m)).unwrap_or(0);

        // Newest-first, stopping at the budget.
        let mut tail: Vec<Message> = Vec::new();
        for m in self.messages[pin..].iter().rev() {
            if Some(&m.content) == self.task.as_ref() && m.role == Role::User {
                continue; // already accounted for above
            }
            let cost = self.tokens_of(m);
            if used + cost > budget {
                break;
            }
            used += cost;
            tail.push(m.clone());
        }
        tail.reverse();
        // A leading tool result would answer a call that is no longer present.
        while tail.first().is_some_and(|m| m.role == Role::Tool) {
            tail.remove(0);
        }

        let dropped = self.messages.len() - (head.len() + usize::from(task.is_some()) + tail.len());
        if dropped == 0 {
            return;
        }
        let mut rebuilt = head;
        if let Some(task) = task {
            rebuilt.push(task);
        }
        rebuilt.extend(tail);
        self.messages = rebuilt;

        // Reported every time, with a count. This used to be a one-shot notice, so a
        // session that kept shedding history looked like it had shed it once — and
        // dropping history is the lossy path, the one worth knowing about repeatedly.
        self.ui.notice(&format!(
            "context window full; dropped {dropped} older message(s) without summarizing"
        ));
    }

    /// Attach a session logger (records transcript, commands, final summary).
    pub fn with_logger(mut self, logger: Option<SessionLogger>) -> Self {
        self.logger = logger;
        self
    }

    /// Attach a dedicated summarizer model for compaction and truncation
    /// recovery. `None` (the default) uses the main model for those calls.
    pub fn with_summarizer(mut self, summarizer: Option<Box<dyn ModelClient>>) -> Self {
        self.summarizer = summarizer;
        self
    }

    /// Append host-provided context (e.g. the memory index) to the system
    /// message so it's always present and never pruned by `fit_context`.
    pub fn with_memory_context(mut self, ctx: String) -> Self {
        if !ctx.trim().is_empty() {
            if let Some(sys) = self.messages.first_mut() {
                sys.content.push_str("\n\n");
                sys.content.push_str(&ctx);
            }
        }
        self
    }

    /// Give the loop this project's named commands and its verification gate.
    ///
    /// Both halves come from `agent.yaml`: `commands` is appended to the system
    /// message so the agent runs *this* project's checks rather than guessing one
    /// from the language, and `verify` arms the `final` gate. The list goes into the
    /// pinned system message (like the memory index) so it survives compaction —
    /// knowing how to test the project is not something to lose on turn 30.
    pub fn with_project_commands(
        mut self,
        commands: &std::collections::BTreeMap<String, String>,
        verify: Vec<String>,
    ) -> Self {
        let mut block = String::new();
        if !commands.is_empty() {
            let list = commands
                .iter()
                .map(|(k, v)| format!("- {k}: `{v}`"))
                .collect::<Vec<_>>()
                .join("\n");
            block.push_str(&format!(
                "\n\nProject commands (from .cowboy/agent.yaml) — prefer these over \
                 guessing a build or test invocation:\n{list}"
            ));
        }
        // Stated even when `commands` is empty: `verify` entries may be literal
        // commands, and the gate must never fire on something the agent was not told
        // about.
        if !verify.is_empty() {
            block.push_str(&format!(
                "\n\nBefore calling `final` on a session that changed files, these must \
                 have been run and passed: {}. Run them with `shell`.",
                verify
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !block.is_empty() {
            if let Some(sys) = self.messages.first_mut() {
                sys.content.push_str(&block);
            }
        }
        self.verification = Verification::new(verify);
        self
    }

    /// Pin the project's own context into the system message: what skills exist, and
    /// what the repo's `AGENTS.md`/`CLAUDE.md` says.
    ///
    /// Both were previously things the agent had to go and fetch — `cowboy skill list`
    /// to find out whether skills existed at all, and a `read` of AGENTS.md to learn
    /// the conventions it is told are authoritative. That is a turn each, on every
    /// session, for information that is small and does not change mid-task; and
    /// because it arrived as a tool result it sat in the compactable middle of the
    /// conversation, so a long session would lose the conventions exactly when it had
    /// accumulated the most code to keep consistent with them. The skill
    /// *instructions* are still fetched on demand — only the index is pinned.
    pub fn with_project_context(mut self, skills_index: &str, instructions: &str) -> Self {
        let mut block = String::new();
        if !skills_index.trim().is_empty() {
            block.push_str("\n\n");
            block.push_str(skills_index.trim_end());
        }
        block.push_str(instructions);
        if !block.is_empty() {
            if let Some(sys) = self.messages.first_mut() {
                sys.content.push_str(&block);
            }
        }
        self
    }

    /// Give the loop the project's declared background processes, so `proc start
    /// <name>` needs no command and the process list is nameable.
    ///
    /// Those marked `auto_start` are started when the session's first turn begins —
    /// honouring a flag that, until the `proc` tool existed, nothing read.
    pub fn with_processes(
        mut self,
        processes: std::collections::BTreeMap<String, cowboy_core::config::ProcessDef>,
    ) -> Self {
        if !processes.is_empty() {
            let list = processes
                .iter()
                .map(|(k, v)| format!("- {k}: `{}`", v.command))
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(sys) = self.messages.first_mut() {
                sys.content.push_str(&format!(
                    "\n\nBackground processes this project defines (start one with the `proc` \
                     tool by name; no `command` needed):\n{list}"
                ));
            }
        }
        self.processes = processes;
        self
    }

    /// Start every `auto_start` process. Best-effort and reported, not fatal: a dev
    /// server that will not come up is something to tell the agent about, not a reason
    /// to refuse the session.
    async fn start_auto_processes(&mut self) {
        let auto: Vec<(String, String, Option<String>)> = self
            .processes
            .iter()
            .filter(|(_, d)| d.auto_start)
            .map(|(n, d)| (n.clone(), d.command.clone(), Some(d.cwd.clone())))
            .collect();
        for (name, command, cwd) in auto {
            match self
                .runtime
                .start_process(&name, &command, cwd.as_deref())
                .await
            {
                Ok(()) => self
                    .ui
                    .notice(&format!("started background process {name} (auto_start)")),
                Err(e) => self
                    .ui
                    .notice(&format!("could not auto-start {name}: {e:#}")),
            }
        }
    }

    /// Seed the conversation with a prior session's history (for resume/
    /// continue), inserted right after the always-kept system message. The new
    /// session keeps its own system prompt; `history` should be system-free
    /// (see [`crate::session::load_history`]).
    pub fn with_history(mut self, history: Vec<Message>) -> Self {
        // Bound what a resume drags in. The transcript on disk is unbounded and has no
        // relationship to the window of whatever model is resuming it, so loading it
        // whole meant the first request either overflowed or immediately paid for a
        // compaction call that threw most of it away. Half the budget leaves room for
        // the turn the user actually came to run.
        let allowance = self.context_budget() / 2;
        let history = Self::tail_within(history, allowance);
        // Insert after messages[0] (system), preserving order, before any task.
        for (i, m) in history.into_iter().enumerate() {
            self.messages.insert(1 + i, m);
        }
        self
    }

    /// The newest messages of `history` that fit in `allowance`, in order.
    ///
    /// Trims from the front, then drops any leading `Tool` message and any leading
    /// assistant turn whose tool calls were left behind — a result with no call, or a
    /// call with no result, is the shape providers reject.
    fn tail_within(history: Vec<Message>, allowance: usize) -> Vec<Message> {
        let mut kept: std::collections::VecDeque<Message> = std::collections::VecDeque::new();
        let mut used = 0usize;
        for m in history.into_iter().rev() {
            let cost = Self::message_tokens(&m);
            if used + cost > allowance && !kept.is_empty() {
                break;
            }
            used += cost;
            kept.push_front(m);
            if used >= allowance {
                break;
            }
        }
        while kept
            .front()
            .is_some_and(|m| m.role == Role::Tool || !m.tool_calls.is_empty())
        {
            kept.pop_front();
        }
        kept.into()
    }

    /// Repair tool-call/tool-result pairing anywhere in the history.
    ///
    /// Providers reject a conversation in which an assistant turn carries a tool call
    /// with no matching result, or a tool result whose call id it has never seen. Both
    /// shapes are fatal for the *whole session*, not just the turn that produced them:
    /// the history is replayed on every subsequent call, so one bad splice 400s until
    /// the session is abandoned.
    ///
    /// Several things reshape the history — `fit_context`, `compact_within_turn`,
    /// `drop_oldest`, `tail_within` on resume, and an interrupted turn — and each was
    /// separately responsible for not breaking the pairing. `seal_dangling_tool_calls`
    /// only repairs the *last* dangling assistant turn, which is the right amount for
    /// the cancel path it was written for and not enough as a general guarantee.
    ///
    /// So the invariant is enforced in one place instead: this runs immediately before
    /// every model call, which is the only point that matters. A path that trims badly
    /// still loses information, but it can no longer produce a conversation the
    /// provider refuses.
    ///
    /// Static rather than a method so it is testable directly on a message list.
    /// Returns how many repairs it made (0 in the normal case).
    fn enforce_tool_call_pairing(messages: &mut Vec<Message>, why: &str) -> usize {
        use std::collections::HashSet;
        let mut out: Vec<Message> = Vec::with_capacity(messages.len());
        let mut repairs = 0usize;
        let mut i = 0usize;
        while i < messages.len() {
            let m = &messages[i];
            if m.role == Role::Assistant && !m.tool_calls.is_empty() {
                let calls = m.tool_calls.clone();
                out.push(m.clone());
                i += 1;
                // The run of tool results that belongs to this assistant turn.
                let mut answered: HashSet<String> = HashSet::new();
                while i < messages.len() && messages[i].role == Role::Tool {
                    let keep = match &messages[i].tool_call_id {
                        // A duplicate result for one call is as invalid as none.
                        Some(id) => {
                            calls.iter().any(|c| &c.id == id) && answered.insert(id.clone())
                        }
                        None => false,
                    };
                    if keep {
                        out.push(messages[i].clone());
                    } else {
                        repairs += 1;
                    }
                    i += 1;
                }
                // Anything still unanswered gets a result, so the turn is complete.
                for c in &calls {
                    if !answered.contains(&c.id) {
                        out.push(Message::tool_result(&c.id, why));
                        repairs += 1;
                    }
                }
            } else if m.role == Role::Tool {
                // A result with no assistant turn before it claiming its id — what a
                // trim that cut mid-turn leaves behind.
                repairs += 1;
                i += 1;
            } else {
                out.push(m.clone());
                i += 1;
            }
        }
        if repairs > 0 {
            *messages = out;
        }
        repairs
    }

    /// Set the active model's per-1M-token USD pricing (used for the running
    /// cost estimate; `None` disables cost tracking for this model).
    pub fn with_pricing(
        mut self,
        input_per_mtok: Option<f64>,
        output_per_mtok: Option<f64>,
    ) -> Self {
        self.price_in = input_per_mtok;
        self.price_out = output_per_mtok;
        self
    }

    /// Set the full pricing triple (including the cached-input rate).
    pub fn with_model_pricing(mut self, pricing: ModelPricing) -> Self {
        self.price_in = pricing.input;
        self.price_out = pricing.output;
        self.price_cached_in = pricing.cached_input;
        self
    }

    /// Register the model to reroute to when the configured one turns out to be
    /// **permanently unavailable** at the provider (a 404 `model_not_found` — e.g.
    /// a roster entry naming a model id the provider has since retired). Without
    /// this, such a model kills the session/subagent outright: the crew's
    /// `fell_back` flag is decided at *routing* time and nothing reroutes on a
    /// runtime error. Used once per session (see `fallback_used`).
    pub fn with_model_fallback(mut self, name: String, build: ModelBuilder) -> Self {
        self.fallback_model = Some((name, build));
        self
    }

    /// Swap the model client (and its context window + pricing) mid-session,
    /// keeping the conversation. Used by the `/model` command.
    pub fn set_model(
        &mut self,
        model: Box<dyn ModelClient>,
        context_window: usize,
        pricing: ModelPricing,
    ) {
        self.model = model;
        self.context_window = context_window;
        self.price_in = pricing.input;
        self.price_out = pricing.output;
        self.price_cached_in = pricing.cached_input_or_input();
    }

    /// Toggle plan mode. While on, `edit`/`write` are refused (the agent must
    /// propose a plan and wait for the user to approve). Used by `/plan` / `/go`.
    pub fn set_planning(&mut self, on: bool) {
        self.planning = on;
    }

    /// Enter wrap-up: the budget is spent, and all that is left is reporting.
    ///
    /// Narrowing the tool surface is the whole point. The directive that goes with
    /// this used to be advisory, and a worker that ignored it kept every tool — one
    /// real subagent answered "stop investigating and report" with fourteen more
    /// `grep`s, was stopped at the ceiling, and lost seventy turns of work because it
    /// had never written anything down. So the surface is narrowed here rather than
    /// asked for in a note.
    ///
    /// Wrap-up is a one-way door (nothing clears `wrapping_up`), so the tool list is
    /// mutated once instead of filtered per request. This is the single place that
    /// happens: six call sites set this state, and a seventh that forgot to narrow
    /// the surface would silently restore the old behaviour.
    fn enter_wrap_up(&mut self) {
        self.wrapping_up = true;
        self.tools
            .retain(|t| tools::allowed_when_wrapping_up(&t.name));
    }

    /// Set the cancellation token used by in-container commands. The worker uses
    /// this so the eager startup setup (`run_session_setup`) is interruptible
    /// before any turn token exists. `run_turn` sets it per turn.
    pub fn set_cancel(&mut self, cancel: CancellationToken) {
        self.cancel = cancel;
    }

    /// Run one conversational turn for `task`, keeping the conversation (and the
    /// session logger) alive for subsequent turns. `turn_cancel` interrupts just
    /// this turn. Does NOT finalize the session.
    pub async fn run_turn(
        &mut self,
        task: &str,
        turn_cancel: CancellationToken,
    ) -> Result<Option<String>> {
        self.cancel = turn_cancel;
        self.run_session_setup().await;
        let outcome = self.run_inner(task).await;
        if let Ok(Some(m)) = &outcome {
            self.last_final = Some(m.clone());
        }
        outcome
    }

    /// One-time per-session setup, run **eagerly when the session comes up** (not
    /// deferred to the first turn) while the UI is live, so the container warms up
    /// immediately. Two steps, both streamed to the transcript with the live
    /// indicator: a *visible* `mise install` (when the workspace uses mise) every
    /// container-up, then the repo's configured `setup` commands run **once per
    /// worktree** (gated by a marker). Best-effort: failures surface but don't
    /// block the session.
    pub async fn run_session_setup(&mut self) {
        if self.setup_done {
            return;
        }
        self.setup_done = true;
        // Subagents share the parent's container/toolchain — only the top-level
        // session does setup.
        if self.subagent_depth > 0 {
            return;
        }

        // When setup will run commands below, bring the container up first,
        // narrating the slow phases (image pull/build, gateway start) — done
        // lazily inside the first command they read as a hang. Best-effort: on
        // failure the setup/turn commands retry and surface the error in context.
        if self.runtime.has_mise_config() || !self.behavior.setup.is_empty() {
            {
                let fut = self.runtime.ensure_running();
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        biased;
                        Some(msg) = self.runtime_status.recv() => self.ui.notice(&msg),
                        res = &mut fut => {
                            if let Err(e) = res {
                                self.ui.notice(&format!("sandbox startup failed: {e:#}"));
                            }
                            break;
                        }
                    }
                }
            }
            self.drain_runtime_status();
        }

        // Toolchain: every container-up (cheap when the mise store is warm).
        if self.runtime.has_mise_config() {
            self.run_setup_command(
                "setting up project toolchain (mise install)…",
                "mise install",
            )
            .await;
        }

        // Repo setup hook: configured commands, run once per worktree. The marker
        // records a hash of the commands, so changing `setup` re-runs them.
        let cmds = self.behavior.setup.clone();
        if cmds.is_empty() {
            return;
        }
        let marker = self
            .runtime
            .root()
            .join(".cowboy")
            .join("sessions")
            .join(".worktree-setup");
        let want = setup_hash(&cmds);
        if std::fs::read_to_string(&marker).is_ok_and(|s| s.trim() == want) {
            return; // this worktree is already set up for these commands
        }
        let mut all_ok = true;
        for cmd in &cmds {
            if self
                .run_setup_command(&format!("running project setup: {cmd}"), cmd)
                .await
                != 0
            {
                all_ok = false;
                break; // a later command likely depends on the failed one
            }
        }
        if all_ok {
            if let Some(dir) = marker.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&marker, want);
        } else {
            self.ui
                .notice("project setup incomplete — it'll retry on the next session");
        }
    }

    /// Forward any queued container bring-up status lines to the UI (see
    /// [`AgentRuntime::status_channel`]). Non-blocking.
    fn drain_runtime_status(&mut self) {
        while let Ok(msg) = self.runtime_status.try_recv() {
            self.ui.notice(&msg);
        }
    }

    /// Run one setup command in the container, streamed with the live indicator.
    /// Returns its exit code (`-1` if it couldn't run); clears the indicator
    /// either way (so a command that never ran doesn't leave the status bar stuck).
    async fn run_setup_command(&mut self, notice: &str, command: &str) -> i32 {
        self.ui.notice(notice);
        let args = ShellArgs {
            command: command.to_string(),
            cwd: None,
            timeout_seconds: None,
        };
        self.ui.command_start(command);
        match self.run_shell_streaming(&args).await {
            Ok((result, _)) => {
                self.ui.command_end(result.exit_code, "");
                result.exit_code
            }
            Err(e) => {
                self.ui.command_end(-1, "");
                self.ui.notice(&format!("`{command}` did not run: {e}"));
                -1
            }
        }
    }

    /// Finalize the session log (diff + summary). Call once when the
    /// conversation ends.
    pub fn finalize_session(&mut self) {
        let status = if self.last_final.is_some() {
            "complete"
        } else {
            "incomplete"
        };
        self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::SessionCompleted {
            status: status.to_string(),
        });
        // Say so if the record of this session is incomplete. The user is about to walk
        // away believing the transcript is what happened, and a full disk is the usual
        // cause — silent truncation of the audit trail is worse than a noisy session.
        let failure = self
            .logger
            .as_ref()
            .and_then(|l| l.write_failure().map(str::to_string));
        if let Some(why) = failure {
            self.ui.notice(&format!(
                "this session's log is incomplete — {why}. The transcript and command \
                 records under .cowboy/sessions/ are missing entries."
            ));
        }
        if let Some(l) = &self.logger {
            l.finalize(self.last_final.as_deref());
        }
    }

    /// The host project root (workspace bind-mount source).
    pub fn root(&self) -> &std::path::Path {
        self.runtime.root()
    }

    /// One-shot convenience: run a single turn then finalize (console mode/tests).
    pub async fn run(&mut self, task: &str) -> Result<Option<String>> {
        let cancel = self.cancel.clone();
        let outcome = self.run_turn(task, cancel).await;
        // A subagent that ended without a clean final would otherwise hand the
        // foreman an empty result, discarding everything it did this turn. Salvage
        // the work into a `[partial]` checkpoint on stdout so the foreman can
        // resume from it instead of restarting the task from scratch.
        if self.subagent_depth > 0 && self.last_final.is_none() {
            if let Some(partial) = self.build_partial_result() {
                self.ui.final_message(&partial);
            }
        }
        self.finalize_session();
        outcome
    }

    /// Assemble whatever a non-finishing subagent managed to do this turn, as a
    /// `[partial]` checkpoint the foreman can resume from: **what it actually
    /// produced** (host-recorded), its latest substantive narration, its plan
    /// progress, and the session id (whose `.cowboy/sessions/<id>/` dir holds the
    /// full transcript, scratchpad, published artifacts, and commands for
    /// recovery). Returns `None` only when there is genuinely nothing to report.
    fn build_partial_result(&self) -> Option<String> {
        let mut sections: Vec<String> = Vec::new();

        // What it produced, first, because it is the only part that is *measured*
        // rather than claimed — and because omitting it threw away finished work.
        //
        // Observed: a worker told to wrap up spent its whole wrap-up allowance doing
        // exactly the right things — wrote a 17.6 KB audit, published it as an
        // artifact, wrote a handoff — and ran out one turn before `final`. The
        // checkpoint then reported only its stale plan, whose last line was
        // "[ ] Write prioritized findings artifact" *for the artifact it had just
        // published*. The foreman read "nothing got done" and re-ran the entire
        // review from scratch. The outputs were on disk the whole time.
        if let Some(l) = &self.logger {
            let dir = l.dir();
            let mut produced: Vec<String> = cowboy_core::artifact::list_in(dir)
                .into_iter()
                .map(|a| {
                    format!(
                        "  {:<8} {}  ({})",
                        a.kind.as_str(),
                        a.title,
                        dir.join(&a.path).display()
                    )
                })
                .collect();
            let handoff = dir.join("handoff.md");
            if handoff.is_file() {
                produced.push(format!("  handoff  written  ({})", handoff.display()));
            }
            if !produced.is_empty() {
                sections.push(format!(
                    "Already produced (recorded by the host, not self-reported — read these \
                     instead of redoing the work):\n{}",
                    produced.join("\n")
                ));
            }
        }

        // The most recent assistant message with real content — usually where the
        // agent was summarizing its findings before the final emission failed.
        if let Some(content) = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
            .map(|m| m.content.trim().to_string())
        {
            sections.push(content);
        }

        // Plan progress: what got done vs. what's left, so resumption can skip
        // completed steps. Explicitly marked as the worker's own account, because a
        // worker that ran out of turns generally ran out *before* ticking the last
        // box — so an unchecked step here is not evidence the work is undone.
        if !self.plan.is_empty() {
            let mut lines =
                String::from("Plan progress (the worker's own last update — may lag what it did):");
            for (step, status) in &self.plan {
                let mark = match status.as_str() {
                    "done" => "[x]",
                    "in_progress" => "[~]",
                    _ => "[ ]",
                };
                lines.push_str(&format!("\n  {mark} {step}"));
            }
            sections.push(lines);
        }

        // Where to recover the rest from (full transcript / scratchpad / commands).
        if let Some(l) = &self.logger {
            sections.push(format!(
                "Checkpoint: session `{}` (.cowboy/sessions/{}/ has the transcript, \
                 scratchpad, and commands run).",
                l.id(),
                l.id()
            ));
        }

        if sections.is_empty() {
            return None;
        }
        Some(format!(
            "[partial] This subagent did not finish cleanly; work so far follows. \
             Resume from this checkpoint rather than restarting.\n\n{}",
            sections.join("\n\n")
        ))
    }

    /// Run the loop for `task` until completion, cancellation, or the iteration
    /// cap. Returns the final message if the agent produced one.
    async fn run_inner(&mut self, task: &str) -> Result<Option<String>> {
        if !self.lifecycle_started {
            self.lifecycle_started = true;
            self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::SessionStarted);
            // Once per session, and only here: `auto_start` means "have this running
            // before the agent starts work", and a later message must not restart it.
            self.start_auto_processes().await;
        }
        let user_msg = Message::user(task);
        if let Some(l) = &mut self.logger {
            l.log_message(&user_msg);
        }
        self.messages.push(user_msg);
        self.task = Some(task.to_string());

        // Fresh budget for this turn. For the foreman that is `max_iterations`, as
        // before; a delegated worker starts on its grant and may earn extensions.
        self.budget.used = 0;
        self.grant_stage_seen = GrantStage::Fine;
        // Per *message*, like the budget it guards: a user who extended an earlier message
        // should not find a later one refusing to ask.
        self.user_extensions = 0;

        // The outer loop exists for the grant-and-request cycle: when the inner loop
        // runs the grant out, a supervised worker reports to its foreman and may be
        // given more, at which point the inner loop resumes. `extend_or_report` only
        // returns true when the budget actually grew, so this cannot spin.
        'grant: loop {
            while !self.budget.exhausted() {
                self.budget.used += 1;
                if self.cancel.is_cancelled() {
                    self.ui.notice("interrupted");
                    return Ok(None);
                }
                // The parent was killed rather than asked to stop: nothing will read
                // this work, so stop spending on it.
                if self.orphaned() {
                    self.ui
                        .notice("the parent session is gone — stopping this subagent");
                    self.seal_dangling_tool_calls("not run: the parent session went away");
                    return Ok(None);
                }

                // Background delegation reports in here, at the one point in the turn where
                // the history can safely grow: a finished subagent's result (or a request
                // for more turns) becomes a message the model sees on this very call.
                self.drain_job_events();
                // Anything the user typed while this turn has been running lands here
                // too, rather than waiting for the turn to end.
                self.drain_steering();

                // Tell the model where it stands *before* it plans the next step, so it
                // can choose to converge (or ask for more turns) while it still has the
                // turns to do either. Injected as a user message, once per stage.
                let stage = grant_stage(self.budget.used, self.budget.granted);
                if stage > self.grant_stage_seen {
                    self.grant_stage_seen = stage;
                    if let Some(note) = grant_notice(stage, &self.budget) {
                        self.ui.notice(&note);
                        let msg = Message::user(note);
                        if let Some(l) = &mut self.logger {
                            l.log_message(&msg);
                        }
                        self.messages.push(msg);
                    }
                }

                // Stop before spending more if a usage budget has been reached.
                if let Some(reason) = self.budget_reached() {
                    self.ui.notice(&reason);
                    return Ok(None);
                }
                self.maybe_warn_budget();

                // Shed old reasoning first: it is the largest re-sent term for a
                // reasoning model, and dropping it is free. Doing it before
                // `fit_context` often means there is nothing left to compact, which
                // saves a summarization call.
                let freed = self.shed_reasoning();
                if freed > 0 && !self.reasoning_shed_notified {
                    self.reasoning_shed_notified = true;
                    self.ui.notice(&format!(
                        "trimmed ~{freed} tokens of older reasoning from the context \
                     (kept the last {REASONING_TURNS_KEPT} turns)"
                    ));
                }

                // Keep history within the model's context window.
                self.fit_context().await;

                // Estimate the prompt tokens actually sent (post-pruning).
                let prompt_est: u64 = self
                    .messages
                    .iter()
                    .map(|m| self.tokens_of(m))
                    .sum::<usize>() as u64;
                // Report what the request costs before making it, so the pressure is
                // visible in the UI and the journal rather than only when it overflows.
                let usage = self.context_usage();
                self.ui.context_usage(&usage);

                let response = match self.call_model().await {
                    Ok(r) => r,
                    Err(_) if self.cancel.is_cancelled() => {
                        self.ui.notice("interrupted");
                        return Ok(None);
                    }
                    // The provider doesn't serve this model (a retired/renamed id in
                    // the roster). Retrying can't help, but another model can: reroute
                    // once and re-run the turn, rather than failing the whole session.
                    Err(e) if self.model_unavailable(&e) && self.try_fallback_model() => continue,
                    Err(e) => {
                        self.ui.notice(&format!("model error: {e}"));
                        return Err(e);
                    }
                };
                self.account_tokens(prompt_est, &response);

                // Record the assistant turn (content + reasoning + any tool calls).
                // Preserving reasoning is what lets agentic reasoning models keep
                // their plan across tool-use turns instead of re-deriving (and
                // looping on) the same step.
                let assistant = Message {
                    role: Role::Assistant,
                    content: response.content.clone().unwrap_or_default(),
                    tool_call_id: None,
                    tool_calls: response.tool_calls.clone(),
                    reasoning: response.reasoning.clone(),
                };
                if let Some(l) = &mut self.logger {
                    l.log_message(&assistant);
                }
                self.messages.push(assistant);

                if response.tool_calls.is_empty() {
                    // No tool call: treat any content as an implicit final answer.
                    let msg = response.content.clone().unwrap_or_default();
                    if !msg.is_empty() {
                        self.ui.final_message(&msg);
                        return Ok(Some(msg));
                    }
                    // Truncated mid-generation with nothing usable: a reasoning model
                    // can spend its entire output budget thinking and never emit an
                    // answer or tool call. Warn once that its output limit may be too
                    // low, then retry: salvage the wasted reasoning into
                    // conclusions-so-far when the provider returned any, and ask the
                    // provider for minimal reasoning effort so the retry cannot spend
                    // the same budget the same way. Bounded by MAX_REPRIME_ATTEMPTS so a
                    // model that always truncates can't spin.
                    if response.truncated {
                        self.warn_output_limit();
                        if self.reprime_attempts < MAX_REPRIME_ATTEMPTS {
                            let directive = self.reprime_directive(&response).await;
                            self.reprime_attempts += 1;
                            // Drop the empty assistant turn we just recorded so the
                            // giant truncated reasoning isn't re-sent; the compact
                            // directive replaces it.
                            self.messages.pop();
                            // Words alone did not work here — "don't think further" is
                            // advice a reasoning model can ignore, and did. Turn the knob
                            // the provider honours for the next call as well.
                            self.minimize_reasoning_next_turn = true;
                            self.ui.notice(
                                "recovering: asking the model to answer without further thinking",
                            );
                            let msg = Message::user(directive);
                            if let Some(l) = &mut self.logger {
                                l.log_message(&msg);
                            }
                            self.messages.push(msg);
                            continue;
                        }
                        // Attempts spent: report it explicitly so the caller (a foreman
                        // reading a subagent's stdout, or the user) sees the cause
                        // instead of a silent empty result.
                        let note = format!(
                            "model hit its output-token limit while reasoning and produced no \
                         answer after {MAX_REPRIME_ATTEMPTS} recovery attempts — raise \
                         max_tokens in models.yaml, or lower reasoning_effort"
                        );
                        self.ui.notice(&note);
                        return Ok(Some(format!("[incomplete] {note}")));
                    }
                    self.ui.notice(
                        "the model didn't return anything to do — rephrase your request, \
                     or try a different model with /model",
                    );
                    return Ok(None);
                }

                // The turn produced a tool call — real progress — so a later
                // truncation gets a fresh reprime budget rather than the tail of an
                // earlier recovery.
                self.reprime_attempts = 0;

                // Coordination-only batches skip both progress guards. A foreman with four
                // workers in flight legitimately calls `jobs` — or `wait` — several times
                // with identical arguments and identical output, which is precisely the
                // shape the repetition guard ends a turn for; and waiting on a worker is not
                // "going in circles", so it must not feed the novelty metric either.
                let coordination = is_coordination_only(&response.tool_calls);

                if !coordination {
                    // Loop guard: an identical tool call that ALSO returns an identical
                    // result makes no progress (a degenerate model loop). Nudge after a few
                    // repeats, abort if it persists — so a runaway costs seconds, not a
                    // hundred API calls.
                    //
                    // The result must be part of the test: a byte-identical call whose
                    // output *changes* is legitimate polling (`sleep 5 && curl health`,
                    // watching a build, waiting on a lock), and keying the guard on the call
                    // alone aborted those runs outright. `obs` is the digest of the previous
                    // iteration's tool results, so comparing it with the one before tells us
                    // whether repeating the call actually changed anything.
                    let sig = tool_signature(&response.tool_calls);
                    let raw_sig = raw_tool_signature(&response.tool_calls);
                    let same_call = self.last_tool_sig.as_deref() == Some(sig.as_str());
                    let same_raw = self.last_raw_tool_sig.as_deref() == Some(raw_sig.as_str());
                    if same_call && !self.last_obs_changed {
                        self.tool_repeat += 1;
                    } else {
                        self.tool_repeat = 0;
                    }
                    // Separately, count *cosmetic churn*: the normalized call is unchanged
                    // but the raw command was edited each turn (a different counter, an
                    // added `echo`, reflowed whitespace). That is the pattern the strict
                    // guard exempts as "polling" because the output changes — yet it is no
                    // progress. Byte-identical repetition (`same_raw`) is real polling and
                    // must NOT count here; it is the strict guard's job.
                    if same_call && !same_raw {
                        self.same_call_repeat += 1;
                    } else {
                        self.same_call_repeat = 0;
                    }
                    self.last_tool_sig = Some(sig);
                    self.last_raw_tool_sig = Some(raw_sig);
                    const LOOP_NUDGE_AT: u32 = 3;
                    const LOOP_ABORT_AT: u32 = 6;
                    // Cosmetic churn escalates in three stages, each stronger than the last,
                    // because the goal is to change the model's behavior — not just to stop.
                    // A model editing one inspection each turn (a different counter, an added
                    // `echo`, reflowed whitespace) gets a nudge, then a forceful directive it
                    // must act on, and only a model that ignores even that is hard-stopped —
                    // still an order of magnitude below max_iterations. `same_call_repeat`
                    // counts only churn (same normalized call, edited raw command).
                    const CHURN_NUDGE_AT: u32 = 8;
                    const CHURN_INTERVENE_AT: u32 = 12;
                    const CHURN_ABORT_AT: u32 = 15;
                    if self.same_call_repeat >= CHURN_ABORT_AT {
                        // It ignored the forceful directive and kept churning. Stop for real.
                        let reps = self.same_call_repeat + 1;
                        self.ui.notice(&format!(
                    "loop detected: same inspection re-run {reps}× with only cosmetic changes and \
                     no real progress, despite an explicit instruction to stop — ending the turn"
                ));
                        for c in &response.tool_calls {
                            self.push_tool_result(
                        &c.id,
                        "[loop guard] aborted: you repeated the same inspection after being told to \
                         stop. The turn is over. No further tool calls will run.",
                    );
                        }
                        return Ok(None);
                    }
                    if self.same_call_repeat >= CHURN_INTERVENE_AT {
                        // Strong intervention: a forceful, specific directive injected as the
                        // tool result, then `continue` so the model actually gets to act on
                        // it. This is the "do something different" message — it forbids the
                        // repeat, spells out the only acceptable next moves, and warns that
                        // ignoring it ends the turn.
                        let reps = self.same_call_repeat + 1;
                        self.ui.notice(
                        "loop guard: STRONG intervention — same inspection churned; ordering a \
                     different action",
                    );
                        for c in &response.tool_calls {
                            self.push_tool_result(
                                &c.id,
                                &format!(
                        "[loop guard — STOP] You have now run essentially this SAME inspection \
                         {reps} times, changing only cosmetic details (a counter, an `echo`, \
                         whitespace, `2>&1`). This is producing NO new information and NO progress \
                         on the task.\n\n\
                         Do NOT run this command — or a variation of it — again. That request will \
                         be refused.\n\n\
                         You have enough information. Take ONE of these actions now:\n\
                         1. State the conclusion you can already draw from the output you have, \
                         then move to the NEXT distinct step of the task.\n\
                         2. If you are blocked, investigate a DIFFERENT file, command, or angle — \
                         not this one.\n\
                         3. If the task is complete, call `final` with your answer.\n\n\
                         If your very next action is another variant of this same inspection, the \
                         turn will be ended immediately."
                    ),
                            );
                        }
                        continue;
                    }
                    if self.same_call_repeat >= CHURN_NUDGE_AT {
                        // First warning: gentle course-correction before the strong directive.
                        let reps = self.same_call_repeat + 1;
                        self.ui.notice(
                    "loop guard: same inspection re-run with cosmetic tweaks — nudging a change of \
                     approach",
                );
                        for c in &response.tool_calls {
                            self.push_tool_result(&c.id, &format!(
                        "[loop guard] You have re-run essentially this same inspection {reps}× with \
                         only cosmetic changes (a different counter, an added `echo`, reflowed \
                         whitespace). This is not progress. Draw a conclusion from what you already \
                         have and move on, or call `final` if the task is complete."
                    ));
                        }
                        continue;
                    }
                    if self.tool_repeat >= LOOP_ABORT_AT {
                        let reps = self.tool_repeat + 1;
                        self.ui.notice(&format!(
                        "loop detected: same action repeated {reps}× with no progress — stopping"
                    ));
                        for c in &response.tool_calls {
                            self.push_tool_result(
                                &c.id,
                                "[loop guard] aborted: identical action repeated with no progress.",
                            );
                        }
                        return Ok(None);
                    }
                    if self.tool_repeat >= LOOP_NUDGE_AT {
                        let reps = self.tool_repeat + 1;
                        self.ui.notice(
                            "loop guard: repeated identical action — nudging a change of approach",
                        );
                        for c in &response.tool_calls {
                            self.push_tool_result(&c.id, &format!(
                        "[loop guard] You have issued this exact command {reps}× and gotten the same \
                         result. STOP repeating it — take a different approach, or call `final` if \
                         the task is complete."
                    ));
                        }
                        continue;
                    }
                }

                let outcome = self.handle_tool_calls(&response).await;
                // Record what this batch actually returned, so the next iteration can
                // tell "same call, same result" (a loop) from "same call, new result"
                // (polling). Done here — after a real execution — and never on the
                // nudge/abort paths, which don't run the tools.
                let obs = self.trailing_observation_sig();
                self.last_obs_changed =
                    self.last_obs_sig.is_some() && obs.is_some() && obs != self.last_obs_sig;
                if obs.is_some() {
                    self.last_obs_sig = obs;
                }
                // Did this iteration learn anything? Answered from what the tools touched,
                // not from what the model says about itself.
                if !coordination {
                    self.progress
                        .observe(&response.tool_calls, self.last_obs_changed);
                }
                match outcome {
                    Ok(Some(final_msg)) => return Ok(Some(final_msg)),
                    Ok(None) => {}
                    Err(e) => {
                        // A tool arm bailed mid-turn: whatever calls it hadn't answered
                        // must still get results, or the next turn ships an assistant
                        // message with dangling tool calls and the provider 400s.
                        self.seal_dangling_tool_calls(
                            "not run: the turn ended early with an internal error",
                        );
                        return Err(e);
                    }
                }
                // Going in circles: several iterations in a row that read nothing new,
                // changed nothing, ran no new command, and saw no changed output. Say so
                // as a user message rather than a tool result — the results for this batch
                // are already recorded, and the directive is for the *next* step.
                if self.progress.stalled(self.stall_window) {
                    self.stall_count += 1;
                    let streak = self.progress.barren_streak();
                    self.progress.clear_streak();
                    self.ui.notice(&format!(
                        "no progress in the last {streak} iterations — intervening"
                    ));
                    // A second stall from a supervised worker is not worth another
                    // directive it has already ignored: escalate to its foreman, with
                    // the measured evidence attached, and let a human-supervised
                    // decision replace the guesswork.
                    if self.stall_count >= 2 && self.control.is_some() && !self.wrapping_up {
                        let report = format!(
                            "This worker appears stuck: {streak} consecutive steps produced \
                             nothing new. Its own account of where it is:\n\n{}",
                            self.build_partial_result()
                                .unwrap_or_else(|| "no reportable progress".to_string())
                        );
                        let asked = self.budget.remaining().max(5);
                        self.ask_for_turns(report, asked).await;
                    } else {
                        let directive = self.stall_directive(streak);
                        self.push_user_note(directive);
                    }
                }
                // The turn may have been cancelled while a tool ran (a cancelled shell
                // exits 130 rather than erroring, so the loop reaches here); seal before
                // the top-of-loop cancel check unwinds us.
                if self.cancel.is_cancelled() {
                    self.seal_dangling_tool_calls("not run: the turn was interrupted");
                }
            }

            // The grant is spent. A supervised worker does not simply stop here: it
            // reports what it has and asks its foreman for more, which is the whole
            // point of a small initial grant. An unsupervised foreman has nobody to
            // ask, so `extend_or_report` returns false and the loop ends as before.
            if self.extend_or_report().await {
                continue 'grant;
            }
            break 'grant;
        }

        self.ui.notice(&format!(
            "reached the iteration budget ({} turns)",
            self.budget.granted
        ));
        Ok(None)
    }

    /// `jobs`: what is running in the background, with the turn usage the foreman needs
    /// to judge an extension request.
    fn run_jobs(&mut self) -> String {
        let views = self.jobs.views();
        if views.is_empty() {
            return "no background jobs have been dispatched in this session.".to_string();
        }
        let mut out = String::from("background jobs:\n");
        for v in &views {
            let secs = v.elapsed_ms / 1000;
            out.push_str(&format!(
                "- `{}` [{}] on {} — {} · {}s · turns {}/{}",
                v.id, v.label, v.model, v.state, secs, v.used, v.granted
            ));
            if v.ceiling > 0 {
                out.push_str(&format!(" (ceiling {})", v.ceiling));
            }
            if v.requested > 0 {
                out.push_str(&format!(" · asking for {} more", v.requested));
            }
            out.push_str(&format!("\n    task: {}\n", v.task));
        }
        let waiting = self.jobs.awaiting_verdict().len();
        if waiting > 0 {
            out.push_str(&format!(
                "{waiting} job(s) are paused waiting for you — answer with `job_reply`.\n"
            ));
        }
        out
    }

    /// `wait`: park until a background job reports, bounded and interruptible.
    ///
    /// Results are delivered automatically at every iteration boundary, so waiting is
    /// never *required* to receive them — this exists only so a foreman with nothing
    /// left to do stops burning turns on filler.
    async fn run_wait(&mut self, args: &tools::WaitArgs) -> String {
        if self.jobs.is_idle() {
            return "nothing to wait for: no background jobs are running.".to_string();
        }
        // Resolve the requested ids; an unknown one is reported rather than silently
        // widening the wait to everything.
        let mut unknown: Vec<String> = Vec::new();
        let targets: Vec<String> = match &args.ids {
            Some(ids) if !ids.is_empty() => ids
                .iter()
                .filter_map(|raw| match self.jobs.resolve_id(raw) {
                    Some(id) => Some(id),
                    None => {
                        unknown.push(raw.clone());
                        None
                    }
                })
                .collect(),
            _ => self
                .jobs
                .outstanding()
                .iter()
                .map(|j| j.id.clone())
                .collect(),
        };
        if targets.is_empty() {
            return format!(
                "no such job(s): {}. Use `jobs` to see what is running.",
                unknown.join(", ")
            );
        }
        let all = args.all.unwrap_or(false);
        let timeout = std::time::Duration::from_secs(
            args.timeout_seconds
                .unwrap_or(WAIT_DEFAULT_SECONDS)
                .clamp(1, WAIT_MAX_SECONDS),
        );
        let deadline = tokio::time::Instant::now() + timeout;
        self.ui.notice(&format!(
            "⏳ waiting for {} background job(s)…",
            targets.len()
        ));

        let settled = |me: &Self| -> bool {
            let done = |id: &String| {
                me.jobs
                    .get(id)
                    .is_none_or(|j| j.state.is_done() || j.report.is_some())
            };
            if all {
                targets.iter().all(done)
            } else {
                targets.iter().any(done)
            }
        };

        let mut landed = 0usize;
        let mut steered = false;
        while !settled(self) {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match self.await_job_news(left).await {
                Woke::News => landed += 1,
                // Acting on what the user just said comes before waiting for a worker.
                Woke::Steered => {
                    steered = true;
                    break;
                }
                Woke::TimedOut => break,
                Woke::Cancelled => return "interrupted while waiting.".to_string(),
            }
        }
        if self.cancel.is_cancelled() {
            return "interrupted while waiting.".to_string();
        }
        let still = self.jobs.outstanding().len();
        if steered {
            format!(
                "stopped waiting: the user said something (above). {still} job(s) are still \
                 running and will report as they land."
            )
        } else if settled(self) {
            format!(
                "{landed} job update(s) arrived — read the messages above for the results \
                 and any turn requests. {still} job(s) still running."
            )
        } else {
            format!(
                "timed out after {}s with {still} job(s) still running. Do something else; \
                 their results will be delivered to you as they land.",
                timeout.as_secs()
            )
        }
    }

    /// `job_reply`: answer a worker that asked for more turns.
    ///
    /// The grant is clamped **here**, host-side, to the job's ceiling: the foreman
    /// decides whether to extend, not how far the bound goes.
    /// The parent side of a job's control channel.
    ///
    /// Keyed by *this* session's id, which is also how the child was told to find it.
    /// Without a logger there is no id to key on, so the child falls through to its own
    /// timeout — which for a turn request is a small extension, and for a question is
    /// proceeding unaided.
    fn job_control_dir(&self, id: &str) -> Option<crate::agent::jobctl::ControlDir> {
        let parent = self.logger.as_ref()?.id().to_string();
        crate::agent::jobctl::ControlDir::create(&parent, id)
    }

    fn run_job_reply(&mut self, args: &tools::JobReplyArgs) -> String {
        use crate::agent::jobctl::Verdict;
        let Some(id) = self.jobs.resolve_id(&args.id) else {
            return format!(
                "no such job `{}`. Use `jobs` to see what is running.",
                args.id
            );
        };
        let Some(job) = self.jobs.get(&id) else {
            return format!("no such job `{id}`.");
        };

        // A question is answered, not adjudicated: it has no budget to clamp and no
        // verdict to write, so it branches out before any of that.
        if let crate::agent::jobs::JobState::AwaitingAnswer { seq } = job.state {
            let label = job.label.clone();
            let Some(answer) = args
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return format!(
                    "job `{id}` is waiting on an answer — put your reply in `instructions`."
                );
            };
            let Some(dir) = self.job_control_dir(&id) else {
                return format!(
                    "could not reach job `{id}` to answer it; it will proceed on its own \
                     judgement once it stops waiting."
                );
            };
            if let Err(e) = dir.write_answer(&crate::agent::jobctl::Answer {
                seq,
                answer: answer.to_string(),
            }) {
                return format!("could not answer job `{id}`: {e}");
            }
            self.jobs.answered(&id);
            self.emit_jobs();
            self.ui
                .notice(&format!("▶ answered subagent {label} ({id})"));
            return format!("answered job `{id}` ({label}); it has resumed.");
        }

        let crate::agent::jobs::JobState::AwaitingVerdict { seq } = job.state else {
            return format!(
                "job `{id}` is not waiting for a verdict (it is {}). Nothing to answer.",
                job.state.as_str()
            );
        };
        // Clamp to the job's remaining headroom. A ceiling of 0 means the job was
        // dispatched without supervision, in which case there is nothing to grant.
        let headroom = job.ceiling.saturating_sub(job.granted);
        let asked = args.iterations.unwrap_or(job.requested).max(1);
        let grantable = asked.min(headroom);
        let label = job.label.clone();

        let verdict = match args.verdict.trim().to_ascii_lowercase().as_str() {
            "grant" | "continue" if grantable > 0 => Verdict::Grant {
                seq,
                iterations: grantable,
            },
            // Nothing left to grant: say so and wrap it up rather than pretending.
            "grant" | "continue" => Verdict::WrapUp { seq },
            "redirect" => {
                let Some(instructions) = args
                    .instructions
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    return "a `redirect` needs `instructions` saying what to do differently."
                        .to_string();
                };
                Verdict::Redirect {
                    seq,
                    iterations: grantable.max(1),
                    instructions: instructions.to_string(),
                }
            }
            "wrap_up" | "wrapup" | "wrap" => Verdict::WrapUp { seq },
            "stop" | "cancel" | "abandon" => Verdict::Stop {
                seq,
                reason: args
                    .instructions
                    .clone()
                    .unwrap_or_else(|| "the foreman stopped this work".to_string()),
            },
            // `answer` is valid, but only for a job that asked a question — which was
            // handled above. Naming it here turns "used the right word at the wrong time"
            // into a specific message instead of "unknown verdict".
            "answer" | "reply" => {
                return format!(
                    "job `{id}` is asking for turns, not asking a question — use `grant`, \
                     `redirect`, `wrap_up`, or `stop`."
                )
            }
            other => {
                return format!(
                    "unknown verdict `{other}`; use `answer` (for a question), `grant`, \
                     `redirect`, `wrap_up`, or `stop`."
                )
            }
        };

        let Some(dir) = self.job_control_dir(&id) else {
            return format!(
                "could not reach job `{id}` to answer it; it will take a small automatic \
                 extension and then wrap up on its own."
            );
        };
        if let Err(e) = dir.write_verdict(&verdict) {
            return format!("could not answer job `{id}`: {e}");
        }
        let kind = verdict.kind();
        let extra = verdict.extra_turns();
        if matches!(verdict, Verdict::Stop { .. }) {
            // Nothing more will come from it, and it is blocked waiting for us — so the
            // registry (and the child) are settled here rather than left to time out.
            self.jobs.stop(&id);
            self.ui
                .notice(&format!("✋ stopped subagent {label} ({id})"));
            self.deliver_job_news();
            return format!("stopped job `{id}`.");
        }
        self.jobs.resume(&id, extra);
        self.ui.notice(&format!(
            "▶ {kind} for subagent {label} ({id}): +{extra} turns"
        ));
        let clamped = if grantable < asked && kind != "wrap_up" {
            format!(
                " (asked for {asked}, clamped to {grantable} by its ceiling of {})",
                self.jobs.get(&id).map(|j| j.ceiling).unwrap_or(0)
            )
        } else {
            String::new()
        };
        format!(
            "answered job `{id}` with `{kind}`: +{extra} turns{clamped}. Its result will \
             be delivered to you when it finishes."
        )
    }

    /// The outcome of asking the foreman for more turns.
    ///
    /// `Continue` means the budget grew; `WrapUp` and `Stop` mean it grew by just
    /// enough to write an answer. There is deliberately no variant for "keep going with
    /// no more turns": every path out of a request leaves the worker able to produce a
    /// result, because the failure this mechanism replaces was a worker that spent
    /// everything and returned nothing.
    async fn ask_for_turns(&mut self, report: String, requested: u32) -> RequestOutcome {
        use crate::agent::jobctl::Verdict;
        let Some(dir) = self.control.clone() else {
            return RequestOutcome::Unavailable;
        };
        if self.turn_requests >= MAX_TURN_REQUESTS {
            self.ui
                .notice("turn-request limit reached — wrapping up with what we have");
            self.enter_wrap_up();
            self.budget.extend(crate::agent::jobctl::WRAP_UP_TURNS);
            return RequestOutcome::WrapUp;
        }
        let seq = self.turn_requests + 1;
        self.turn_requests = seq;
        let requested = requested.clamp(1, self.budget.ceiling.max(1));
        let req = crate::agent::jobctl::TurnRequest {
            seq,
            report,
            // The host's own account, attached to the worker's. A foreman judging an
            // extension needs to know whether anything actually happened, and that is
            // not something the asking worker is a reliable narrator of.
            evidence: format!(
                "{} · turns {}/{} (ceiling {})",
                self.progress.evidence(),
                self.budget.used,
                self.budget.granted,
                self.budget.ceiling
            ),
            requested,
            used: self.budget.used,
            granted: self.budget.granted,
        };
        if let Err(e) = dir.write_request(&req) {
            self.ui.notice(&format!(
                "could not reach the foreman to ask for turns: {e}"
            ));
            return RequestOutcome::Unavailable;
        }
        self.ui.notice(&format!(
            "⏸ asked the foreman for {requested} more turns (used {}/{})",
            self.budget.used, self.budget.granted
        ));

        // Poll for the answer. Bounded, interruptible, and never fatal on timeout.
        let deadline = tokio::time::Instant::now() + self.request_timeout;
        let verdict = loop {
            if self.cancel.is_cancelled() {
                return RequestOutcome::Interrupted;
            }
            // The longest a worker sits idle, so the most valuable place to notice that
            // the foreman it is waiting on no longer exists.
            if self.orphaned() {
                self.ui
                    .notice("the parent session is gone — nothing will answer this request");
                return RequestOutcome::Interrupted;
            }
            if let Some(v) = dir.read_verdict(seq) {
                break Some(v);
            }
            if tokio::time::Instant::now() >= deadline {
                break None;
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return RequestOutcome::Interrupted,
                _ = tokio::time::sleep(REQUEST_POLL) => {}
            }
        };

        match verdict {
            Some(Verdict::Grant { iterations, .. }) => {
                let added = self.budget.extend(iterations);
                if added == 0 {
                    self.ui.notice(
                        "the foreman granted more turns but the host ceiling is reached — \
                         wrapping up",
                    );
                    self.enter_wrap_up();
                    self.push_user_note(WRAP_UP_DIRECTIVE.to_string());
                    return RequestOutcome::WrapUp;
                }
                self.ui.notice(&format!("▶ granted {added} more turns"));
                self.progress.clear_streak();
                self.push_user_note(format!(
                    "[foreman] Granted {added} more turns. Continue from where you are — do \
                     not restart, and do not re-read what you have already read."
                ));
                RequestOutcome::Continue
            }
            Some(Verdict::Redirect {
                iterations,
                instructions,
                ..
            }) => {
                let added = self.budget.extend(iterations.max(1));
                self.ui
                    .notice(&format!("▶ redirected with {added} more turns"));
                // A redirect is a fresh start on a different track, so the stall streak
                // that may have triggered this must not immediately re-fire.
                self.progress.clear_streak();
                self.push_user_note(format!(
                    "[foreman] Change of direction, with {added} more turns. Do this \
                     instead:\n\n{instructions}"
                ));
                if added == 0 {
                    self.enter_wrap_up();
                    self.push_user_note(WRAP_UP_DIRECTIVE.to_string());
                    return RequestOutcome::WrapUp;
                }
                RequestOutcome::Continue
            }
            Some(Verdict::WrapUp { .. }) => {
                let added = self.budget.extend(crate::agent::jobctl::WRAP_UP_TURNS);
                self.ui
                    .notice(&format!("▣ told to wrap up ({added} turns to write it up)"));
                self.enter_wrap_up();
                self.push_user_note(WRAP_UP_DIRECTIVE.to_string());
                RequestOutcome::WrapUp
            }
            Some(Verdict::Stop { reason, .. }) => {
                self.ui
                    .notice(&format!("✋ the foreman stopped this: {reason}"));
                self.enter_wrap_up();
                self.budget.extend(crate::agent::jobctl::WRAP_UP_TURNS);
                self.push_user_note(format!(
                    "[foreman] Stop this work now: {reason}\n\nDo not investigate or change \
                     anything further. Call `final` immediately, stating what you had \
                     established and that the work was stopped."
                ));
                RequestOutcome::Stop
            }
            // Unanswered. One small extension, then wrap up.
            //
            // This is the one policy decision in the mechanism that matters most.
            // Continuing indefinitely would restore the runaway this exists to prevent
            // whenever the foreman is idle; stopping outright would destroy a worker's
            // work over an unanswered message. A bounded extension, then a wrap-up,
            // terminates *and* keeps the work.
            None => {
                if self.auto_extensions == 0 {
                    self.auto_extensions += 1;
                    let added = self.budget.extend(AUTO_EXTENSION_TURNS);
                    if added > 0 {
                        self.ui.notice(&format!(
                            "no answer from the foreman — taking {added} more turns, then \
                             wrapping up"
                        ));
                        self.push_user_note(format!(
                            "[no answer] The foreman did not respond, so you have {added} more \
                             turns and no more after that. Get to a reportable state now: \
                             finish the most valuable remaining piece, then write up what you \
                             have."
                        ));
                        return RequestOutcome::Continue;
                    }
                }
                self.ui
                    .notice("no answer from the foreman — wrapping up with what we have");
                self.enter_wrap_up();
                self.budget.extend(crate::agent::jobctl::WRAP_UP_TURNS);
                self.push_user_note(WRAP_UP_DIRECTIVE.to_string());
                RequestOutcome::WrapUp
            }
        }
    }

    /// `request_turns`: the worker asks for more, on its own initiative.
    async fn run_request_turns(&mut self, args: &tools::RequestTurnsArgs) -> String {
        let report = format!(
            "Progress: {}\nRemaining: {}\nNext step: {}",
            args.progress.trim(),
            args.remaining.trim(),
            args.next_step.trim()
        );
        match self.ask_for_turns(report, args.iterations).await {
            RequestOutcome::Continue => format!(
                "granted: you now have {} turns ({} used). Continue from where you are.",
                self.budget.granted, self.budget.used
            ),
            RequestOutcome::WrapUp => format!(
                "not granted. You have {} turns left — write up what you have and call \
                 `final` now.",
                self.budget.remaining()
            ),
            RequestOutcome::Stop => {
                "the foreman stopped this work. Call `final` now, reporting what you had \
                 established."
                    .to_string()
            }
            RequestOutcome::Unavailable => {
                "there is no foreman to ask on this run. Work within the turns you have \
                 and finish with `final`."
                    .to_string()
            }
            RequestOutcome::Interrupted => "interrupted while asking for more turns.".to_string(),
        }
    }

    /// The grant is spent. A supervised worker reports and asks rather than simply
    /// stopping; returns true when it may keep going (the budget grew).
    async fn extend_or_report(&mut self) -> bool {
        if self.control.is_none() {
            // No foreman above this session — so ask the person. Same shape, one level up.
            return self.ask_user_to_continue().await;
        }
        // Already told to finish: it was given turns to write an answer, and asking
        // again would turn "wrap up" into an unbounded extension.
        if self.wrapping_up {
            return false;
        }
        let before = self.budget.granted;
        // The worker did not ask, so the host asks on its behalf, using what it can see:
        // the worker's own latest narration and plan, plus the measured evidence.
        let report = format!(
            "The worker's turn grant is spent and it did not ask for more. Its state:\n\n{}",
            self.build_partial_result()
                .unwrap_or_else(|| "no reportable progress".to_string())
        );
        self.ui
            .notice("turn grant spent — reporting to the foreman");
        let asked = self.budget.granted.max(1);
        let outcome = self.ask_for_turns(report, asked).await;
        if matches!(outcome, RequestOutcome::Interrupted) {
            return false;
        }
        self.budget.granted > before
    }

    /// The foreman's budget is spent: ask the user whether to keep going.
    ///
    /// A delegated worker reports to its foreman and may be granted more turns. The
    /// foreman has no foreman — and the previous behaviour was to print "reached the
    /// iteration budget" and end the turn. That is recoverable (typing anything starts a
    /// new turn with the conversation intact) but nothing says so, so the session looks
    /// finished when it is merely paused. Observed on a real review: the answer was
    /// "keep going", which is precisely the question worth asking.
    ///
    /// Asking is the right primitive rather than raising the cap, because the cap is doing
    /// its job — it caught a long run and handed the decision to a human, which is the
    /// same escalation the worker path makes.
    ///
    /// Fails closed in three ways, so nothing can hang or spin:
    /// - No one to answer (piped run, no attached client) → `ask_user` returns empty →
    ///   stop, exactly as before.
    /// - Anything other than an affirmative → stop.
    /// - Bounded by [`MAX_USER_EXTENSIONS`], so "yes" cannot become an infinite loop.
    async fn ask_user_to_continue(&mut self) -> bool {
        if self.cancel.is_cancelled() {
            return false;
        }
        if self.user_extensions >= MAX_USER_EXTENSIONS {
            self.ui.notice(&format!(
                "reached the iteration budget ({} turns) and has been extended \
                 {MAX_USER_EXTENSIONS}× — stopping. Send a message to continue.",
                self.budget.granted
            ));
            return false;
        }
        let used = self.budget.used;
        let question = format!(
            "The agent has used its {used}-turn budget for this message and is not done. \
             Keep going?"
        );
        let answer = self
            .ui
            .ask_user(&question, &["yes".to_string(), "no".to_string()]);
        if !is_affirmative(&answer) {
            self.ui.notice(&format!(
                "reached the iteration budget ({used} turns) — stopping here. \
                 Send a message to continue.",
            ));
            return false;
        }
        self.user_extensions += 1;
        // One more *round*, not double the last one. Extending by the current grant would
        // double each time (100 → 200 → 400 …), so "yes" would quietly escalate and the
        // extension cap would bound something enormous. A constant step matches what the
        // answer means: keep going for another budget's worth.
        let step = if self.behavior.max_iterations > 0 {
            self.behavior.max_iterations
        } else {
            self.budget.granted.max(1)
        };
        let added = self.budget.extend_with_consent(step);
        self.ui
            .notice(&format!("▶ continuing with {added} more turns"));
        self.progress.clear_streak();
        // Into the conversation, not just the UI: the model needs to know it was extended
        // and that it is expected to converge, or it resumes as if nothing happened.
        self.push_user_note(format!(
            "[user] Granted {added} more turns. Continue from where you are — do not \
             restart, and do not re-read what you have already read. Converge on an \
             answer and call `final`."
        ));
        true
    }
    /// A one-line description of what is still running, for the `final` refusal.
    fn outstanding_jobs_summary(&self) -> String {
        let jobs = self.jobs.outstanding();
        let names: Vec<String> = jobs
            .iter()
            .map(|j| format!("`{}` [{}]", j.id, j.label))
            .collect();
        match names.len() {
            0 => "no subagents are".to_string(),
            1 => format!("subagent {} is", names[0]),
            n => format!("{n} subagents ({}) are", names.join(", ")),
        }
    }

    /// The stall intervention text. Escalates: the first one asks for a change of
    /// approach, a later one says to stop exploring altogether, because a directive a
    /// model has already ignored once is not worth repeating verbatim.
    fn stall_directive(&self, streak: u32) -> String {
        let evidence = self.progress.evidence();
        let base = format!(
            "[no progress] The last {streak} steps produced nothing new — no new file read, \
             no edit, no new command, and no changed output. Measured so far: {evidence}."
        );
        if self.stall_count <= 1 {
            format!(
                "{base}\n\nStop repeating what you have already done. Take one concrete \
                 different action now: inspect a file or symbol you have NOT looked at, run a \
                 command you have NOT run, make the edit the task calls for, or — if you \
                 already know the answer — write it up and call `final`."
            )
        } else if self.budget.supervised {
            format!(
                "{base}\n\nThis is the second time. Do not continue exploring. Either make the \
                 change the task asks for, or report honestly — what you have established, \
                 what is left, why you are stuck — and call `final` with that. An unreported \
                 loop wastes the whole delegation."
            )
        } else {
            format!(
                "{base}\n\nThis is the second time. Write up what you have established, state \
                 plainly what you could not determine, and call `final` now."
            )
        }
    }

    /// Digest of the tool results at the end of the history — i.e. what the
    /// previous iteration's tool calls actually returned. `None` when the tail
    /// isn't a tool-result run (nothing to compare). Used by the loop guard to tell
    /// a stuck model from legitimate polling.
    fn trailing_observation_sig(&self) -> Option<String> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let obs: Vec<&str> = self
            .messages
            .iter()
            .rev()
            .take_while(|m| m.role == Role::Tool)
            .map(|m| m.content.as_str())
            .collect();
        if obs.is_empty() {
            return None;
        }
        let mut h = DefaultHasher::new();
        obs.hash(&mut h);
        Some(format!("{:016x}", h.finish()))
    }

    /// Whether this error means the configured model doesn't exist at the provider
    /// (permanent — see `Error::ModelUnavailable`).
    fn model_unavailable(&self, e: &anyhow::Error) -> bool {
        matches!(
            e.downcast_ref::<cowboy_core::error::Error>(),
            Some(cowboy_core::error::Error::ModelUnavailable(_))
        )
    }

    /// Reroute to the configured fallback model (once). Returns whether the swap
    /// happened, so the caller can retry the turn; `false` means there's no
    /// fallback, it's already been used, or it failed to build — in which case the
    /// original error surfaces as before.
    fn try_fallback_model(&mut self) -> bool {
        if self.fallback_used {
            return false;
        }
        let Some((name, build)) = &self.fallback_model else {
            return false;
        };
        let name = name.clone();
        match build(&name) {
            Ok((client, cw, pricing)) => {
                self.fallback_used = true;
                self.set_model(client, cw, pricing);
                self.ui.notice(&format!(
                    "the configured model is not available at the provider — \
                     falling back to `{name}` and retrying (fix the model id in \
                     models.yaml/crew.yaml to silence this)"
                ));
                self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::ModelFallback {
                    model: name,
                });
                true
            }
            Err(e) => {
                self.fallback_used = true; // don't spin on a broken fallback
                self.ui
                    .notice(&format!("fallback model `{name}` could not be built: {e}"));
                false
            }
        }
    }

    /// Push a tool-result message (logged, capped, and added to history).
    ///
    /// **The single place a tool result enters the conversation**, and the single place
    /// the size cap is applied. It used to be one of several ways in, with each arm
    /// responsible for its own truncation — and three of them were not: `subagent`
    /// (a whole child process's stdout, verbatim, times however many ran in parallel),
    /// `mcp` (bytes from a third-party server, including full JSON schemas from
    /// `list_tools`), and `memory recall` (whole memory bodies). Any one of those could
    /// put an arbitrary amount of text into the context in a single turn.
    ///
    /// Capping here rather than per-arm makes it structural: a new tool cannot forget.
    /// Results already truncated by their handler (shell, the file tools) pass through
    /// unchanged, since this uses the same limit.
    fn push_tool_result(&mut self, tool_call_id: &str, content: &str) {
        let capped = support::truncate(content, self.behavior.max_command_output_bytes);
        let msg = Message::tool_result(tool_call_id, capped);
        if let Some(l) = &mut self.logger {
            l.log_message(&msg);
        }
        self.messages.push(msg);
    }

    /// Answer tool calls that will never run (the turn ended early), so the
    /// assistant message that carried them has a result for **every** call id.
    /// Providers reject a conversation containing an assistant turn with an
    /// unanswered tool call, and this history is replayed on every later turn.
    fn answer_unrun(&mut self, calls: &[cowboy_core::model::ToolCall], why: &str) {
        for call in calls {
            self.push_tool_result(&call.id, why);
        }
    }

    /// Repair the message tail so it never ends with an assistant turn whose tool
    /// calls lack results — the shape providers reject. Called before a turn is
    /// abandoned (cancel/error), where the loop may have pushed the assistant
    /// message but not yet every tool result. Mirrors `session::sanitize_history`,
    /// which does the same on resume; without it, an interrupted turn poisons the
    /// live conversation and every subsequent turn 400s.
    fn seal_dangling_tool_calls(&mut self, why: &str) {
        let Some(idx) = self
            .messages
            .iter()
            .rposition(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        else {
            return;
        };
        let answered: std::collections::HashSet<String> = self.messages[idx + 1..]
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        let unanswered: Vec<cowboy_core::model::ToolCall> = self.messages[idx]
            .tool_calls
            .iter()
            .filter(|c| !answered.contains(&c.id))
            .cloned()
            .collect();
        self.answer_unrun(&unanswered, why);
    }

    /// Process this turn's tool calls. Returns `Some(message)` if `final` was
    /// called.
    async fn handle_tool_calls(&mut self, response: &ChatResponse) -> Result<Option<String>> {
        // Pre-pass: start every delegation in this turn *in the background*, so the
        // rest of the turn (and the next model call) proceeds while they run. Each call
        // is answered with its job id; the actual results are injected at a later
        // iteration boundary. Skipped entirely while planning: the pre-pass would
        // otherwise spawn workers (which DO edit files) before the plan-mode gate
        // below ever runs.
        let sub_results = if self.planning {
            Default::default()
        } else {
            self.dispatch_subagents(&response.tool_calls)
        };

        for (i, call) in response.tool_calls.iter().enumerate() {
            // Plan mode gate: refuse the tools that can change the workspace, so the
            // agent proposes a plan instead of doing the work. Host-enforced —
            // independent of the prompt. `shell` is included because it is trivially
            // mutating (`sed -i`, `git commit`, `rm`), and `subagent` because a child
            // worker is not bound by this session's plan mode.
            if self.planning
                && matches!(
                    call.name.as_str(),
                    tools::TOOL_EDIT | tools::TOOL_WRITE | tools::TOOL_SHELL | tools::TOOL_SUBAGENT
                )
            {
                self.push_tool_result(
                    &call.id,
                    "blocked: plan mode is on — do not modify files, run commands, or \
                     delegate work yet. Present your plan (use the `plan` tool to list \
                     the steps), then stop; the user will approve with /go before you \
                     make changes. Use `read`, `grep`, and `ls` to investigate.",
                );
                continue;
            }
            // Wrap-up gate: the budget is spent and the only thing left is to report,
            // so investigation and mutation are refused here as well as removed from
            // the offered surface. Both layers are needed — the conversation history
            // is full of earlier `shell`/`read` calls, and a model will reach for a
            // tool it used ten turns ago whether or not it is still on offer.
            if self.wrapping_up && !tools::allowed_when_wrapping_up(call.name.as_str()) {
                self.push_tool_result(
                    &call.id,
                    "blocked: you are out of turns and this is the wrap-up. No more \
                     investigating or editing — everything you can still report is \
                     already in this conversation. Call `final` now with what you \
                     established, what you did not get to, and what the next worker \
                     needs. An unreported result is a wasted delegation.",
                );
                continue;
            }
            match call.name.as_str() {
                tools::TOOL_FINAL => {
                    let Some(args) = self.parse_or_report::<FinalArgs>(call) else {
                        continue;
                    };
                    // Finishing with delegated work still in flight throws it away —
                    // the results would arrive after the turn that wanted them. Refuse
                    // and say what is outstanding.
                    //
                    // Bounded, because a hard refusal loop is the one way this gate
                    // could wedge a session: after a couple of refusals the loop stops
                    // arguing and waits on the foreman's behalf.
                    if !self.jobs.is_idle() {
                        if self.final_refusals < MAX_FINAL_REFUSALS {
                            self.final_refusals += 1;
                            let outstanding = self.outstanding_jobs_summary();
                            self.push_tool_result(
                                &call.id,
                                &format!(
                                    "blocked: {outstanding} still running. Their results are \
                                     part of this task and will be delivered to you as they \
                                     land. Either keep working on something else, or call \
                                     `wait` — then finish once you have them."
                                ),
                            );
                            self.answer_unrun(
                                &response.tool_calls[i + 1..],
                                "not run: `final` was refused while subagents are still running",
                            );
                            continue;
                        }
                        self.ui
                            .notice("waiting for running subagents before finishing…");
                        self.await_job_news(FINAL_AUTO_WAIT).await;
                        if !self.jobs.is_idle() {
                            // Still not done after the wait: let the answer stand rather
                            // than blocking forever, but say so plainly.
                            self.ui.notice(
                                "finishing with subagents still running — their results will \
                                 arrive in a later turn",
                            );
                        }
                    }
                    // The project nominated checks (`agent.verify`) and this session
                    // changed files without them passing. Refuse, naming exactly what
                    // to run — the evidence is host-recorded, so this cannot be
                    // satisfied by asserting it in the summary.
                    //
                    // Bounded like the gate above, and for the same reason: quality
                    // pressure must not be able to wedge a session. A check that is
                    // genuinely broken (no network, missing toolchain) would otherwise
                    // trap the agent in a loop it cannot exit.
                    if self.verification.has_unverified_edits() {
                        if self.final_refusals < MAX_FINAL_REFUSALS {
                            self.final_refusals += 1;
                            let outstanding = self
                                .verification
                                .outstanding()
                                .iter()
                                .map(|c| format!("`{c}`"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            self.push_tool_result(
                                &call.id,
                                &format!(
                                    "blocked: this session changed files but {outstanding} \
                                     has not passed against the current tree. Run it with \
                                     `shell` and fix what it reports, then call `final`. If \
                                     it cannot run here, say so in your `final` message and \
                                     call `final` again."
                                ),
                            );
                            self.answer_unrun(
                                &response.tool_calls[i + 1..],
                                "not run: `final` was refused while edits are unverified",
                            );
                            continue;
                        }
                        // Budget spent: accept, but record that the work is unchecked
                        // rather than letting it look verified.
                        self.ui.notice(&format!(
                            "finishing with unverified edits — {} did not pass",
                            self.verification.outstanding().join(", ")
                        ));
                    }
                    if let Some(l) = &self.logger {
                        l.write_final(&args.message);
                    }
                    self.ui.final_message(&args.message); // Answer this call and any the model batched after it. An
                                                          // assistant turn whose tool calls aren't all answered is
                                                          // rejected by strict providers on the NEXT turn (this history
                                                          // persists across turns), which would brick the session.
                    self.push_tool_result(&call.id, "final answer recorded.");
                    self.answer_unrun(
                        &response.tool_calls[i + 1..],
                        "not run: the agent ended the turn with `final`",
                    );
                    return Ok(Some(args.message));
                }
                tools::TOOL_SHELL => {
                    let Some(args) = self.parse_or_report::<ShellArgs>(call) else {
                        continue;
                    };
                    self.ui.command_start(&args.command);
                    let started = std::time::Instant::now();
                    let timeout_secs = self.shell_timeout(&args);
                    // A container/exec failure must NOT propagate with `?`: that
                    // would return from the turn leaving this call unanswered and
                    // corrupt the conversation for every later turn. Report it to
                    // the model as a tool result instead (as `run_fileop` does) and
                    // let it decide what to do.
                    let (result, output) = match self.run_shell_streaming(&args).await {
                        Ok(v) => v,
                        Err(e) => {
                            self.ui.command_end(-1, "");
                            self.push_tool_result(
                                &call.id,
                                &format!("error: the command could not be run: {e:#}"),
                            );
                            continue;
                        }
                    };
                    let duration_ms = started.elapsed().as_millis();
                    self.ui.command_end(result.exit_code, "");
                    if let Some(l) = &mut self.logger {
                        l.log_command(&args.command, result.exit_code, duration_ms, &output);
                    }
                    // Host-recorded verification evidence: the loop notes what ran and
                    // how it exited, so the `final` gate measures rather than trusting
                    // the model's account of what it checked.
                    self.verification
                        .note_command(&args.command, result.exit_code);
                    // A timeout or a cancel is not a real exit status, and neither is
                    // actionable as a bare number. The note goes *after* truncation and
                    // its length comes out of the budget, so the cap cannot shear off
                    // the one part of the observation that says what to do next.
                    let note = shell_outcome_note(
                        result.exit_code,
                        timeout_secs,
                        MAX_SHELL_TIMEOUT_SECONDS,
                    );
                    // Truncate the assembled observation, not just the output: the
                    // `[exit code]` prefix has to be inside the budget or
                    // `push_tool_result` truncates again — head-only — and cuts off
                    // the tail `truncate_middle` just preserved. The prefix is at the
                    // head, which middle truncation always keeps.
                    let observation = format!(
                        "[exit code: {} · {}]\n{}",
                        result.exit_code,
                        fmt_duration(duration_ms),
                        output
                    );
                    let mut observation = truncate_middle(
                        &observation,
                        self.behavior
                            .max_command_output_bytes
                            .saturating_sub(note.len()),
                    );
                    observation.push_str(&note);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_READ => {
                    let Some(args) = self.parse_or_report::<ReadArgs>(call) else {
                        continue;
                    };
                    self.ui.tool_use(&format!("read {}", args.path));
                    let payload = serde_json::json!({
                        "op": "read", "path": args.path,
                        "offset": args.offset, "limit": args.limit,
                    });
                    // Read it, then decide whether the *content* is worth spending
                    // context on. Re-reading an unchanged file is the cheapest way for
                    // a model to look busy — observed burning a whole grant on it — so
                    // an identical re-read is answered with a pointer to the earlier
                    // one instead of a second copy. The read still happens: that is
                    // what proves it is unchanged.
                    let (code, output, observation) =
                        self.fileop_observation(&payload, Trim::Read).await;
                    let observation = if code == 0 {
                        let key = ProgressTracker::read_key(&args.path, args.offset, args.limit);
                        match self.progress.note_read(&key, &output, self.budget.used) {
                            Some(prior) => {
                                self.ui.notice(&format!(
                                    "↺ {} unchanged since step {prior} — not re-reading",
                                    args.path
                                ));
                                reread_notice(&args.path, prior)
                            }
                            None => observation,
                        }
                    } else {
                        observation
                    };
                    // The host now knows this file's content as the agent saw it, which
                    // is what makes a later blind overwrite detectable.
                    if code == 0 {
                        self.seen_files
                            .note(&args.path, &self.read_workspace_file(&args.path));
                    }
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_EDIT => {
                    let Some(args) = self.parse_or_report::<EditArgs>(call) else {
                        continue;
                    };
                    let before = self.read_workspace_file(&args.path);
                    let payload = serde_json::json!({
                        "op": "edit", "path": args.path,
                        "old": args.old, "new": args.new, "replace_all": args.replace_all,
                        "edits": args.edits.iter().map(|e| serde_json::json!({
                            "old": e.old, "new": e.new, "replace_all": e.replace_all,
                        })).collect::<Vec<_>>(),
                    });
                    let (exit, out, observation) =
                        self.fileop_observation(&payload, Trim::Head).await;
                    self.ui
                        .tool_use(&fileop_summary("edit", &args.path, exit, &out));
                    let observation = self.after_file_change(&args.path, before, exit, observation);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_WRITE => {
                    let Some(args) = self.parse_or_report::<WriteArgs>(call) else {
                        continue;
                    };
                    let before = self.read_workspace_file(&args.path);
                    // A full-file overwrite destroys whatever is there; refuse when
                    // what is there is not what the agent last saw.
                    if let Some(refusal) = self.stale_write_refusal(&args.path, before.as_deref()) {
                        self.ui.tool_use(&format!("write {} — refused", args.path));
                        self.push_tool_result(&call.id, &refusal);
                        continue;
                    }
                    let payload = serde_json::json!({
                        "op": "write", "path": args.path, "content": args.content,
                    });
                    let (exit, out, observation) =
                        self.fileop_observation(&payload, Trim::Head).await;
                    self.ui
                        .tool_use(&fileop_summary("write", &args.path, exit, &out));
                    let observation = self.after_file_change(&args.path, before, exit, observation);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_PROC => {
                    let Some(args) = self.parse_or_report::<tools::ProcArgs>(call) else {
                        continue;
                    };
                    let out = self.run_proc(&args).await;
                    self.ui.tool_use(&format!(
                        "proc {}{}",
                        args.action,
                        args.name
                            .as_deref()
                            .map(|n| format!(" {n}"))
                            .unwrap_or_default()
                    ));
                    self.push_tool_result(&call.id, &out);
                }
                tools::TOOL_GREP => {
                    let Some(args) = self.parse_or_report::<GrepArgs>(call) else {
                        continue;
                    };
                    let payload = serde_json::json!({
                        "op": "grep", "pattern": args.pattern, "path": args.path,
                        "glob": args.glob, "literal": args.literal,
                        "case_insensitive": args.case_insensitive,
                        "max_results": args.max_results,
                        "context": args.context, "files_only": args.files_only,
                        "include_ignored": args.include_ignored,
                    });
                    let summary = match (&args.path, &args.glob) {
                        (Some(p), Some(g)) => format!("grep {:?} in {p} ({g})", args.pattern),
                        (Some(p), None) => format!("grep {:?} in {p}", args.pattern),
                        (None, Some(g)) => format!("grep {:?} ({g})", args.pattern),
                        (None, None) => format!("grep {:?}", args.pattern),
                    };
                    self.ui.tool_use(&summary);
                    let _ = self.run_fileop(&call.id, &payload).await?;
                }
                tools::TOOL_LS => {
                    let Some(args) = self.parse_or_report::<tools::LsArgs>(call) else {
                        continue;
                    };
                    let payload = serde_json::json!({
                        "op": "list", "path": args.path, "glob": args.glob,
                        "recursive": args.recursive, "max_results": args.max_results,
                        "include_ignored": args.include_ignored,
                    });
                    let summary = match (&args.path, &args.glob) {
                        (Some(p), Some(g)) => format!("ls {p} ({g})"),
                        (Some(p), None) => format!("ls {p}"),
                        (None, Some(g)) => format!("ls ({g})"),
                        (None, None) => "ls".to_string(),
                    };
                    self.ui.tool_use(&summary);
                    let _ = self.run_fileop(&call.id, &payload).await?;
                }
                tools::TOOL_MEMORY => {
                    let Some(args) = self.parse_or_report::<MemoryArgs>(call) else {
                        continue;
                    };
                    self.ui.tool_use(&format!("memory {}", args.action));
                    let observation = self.run_memory(&args);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_PLAN => {
                    let Some(args) = self.parse_or_report::<PlanArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_plan(args);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_JOBS => {
                    self.ui.tool_use("jobs");
                    let observation = self.run_jobs();
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_WAIT => {
                    let Some(args) = self.parse_or_report::<tools::WaitArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_wait(&args).await;
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_JOB_REPLY => {
                    let Some(args) = self.parse_or_report::<tools::JobReplyArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_job_reply(&args);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_REQUEST_TURNS => {
                    let Some(args) = self.parse_or_report::<tools::RequestTurnsArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_request_turns(&args).await;
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_ARTIFACT => {
                    let Some(args) = self.parse_or_report::<ArtifactArgs>(call) else {
                        continue;
                    };
                    self.ui.tool_use(&format!("artifact {}", args.action));
                    let observation = self.run_artifact(&args);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_HANDOFF => {
                    let Some(args) = self.parse_or_report::<HandoffArgs>(call) else {
                        continue;
                    };
                    self.ui.tool_use("handoff");
                    let observation = self.run_handoff(&args);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_REQUEST_PATH => {
                    let Some(args) = self.parse_or_report::<RequestPathArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_request_path(&args);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_BLOCKED => {
                    let Some(args) = self.parse_or_report::<BlockedArgs>(call) else {
                        continue;
                    };
                    self.ui.blocked(Some(&args.reason));
                    self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::Blocked {
                        reason: args.reason.clone(),
                        waiting_on: args.waiting_on.clone().unwrap_or_default(),
                    });
                    let observation = format!("marked blocked: {}", args.reason);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_DECISION => {
                    let Some(args) = self.parse_or_report::<DecisionArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_decision(&args);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_UNBLOCK => {
                    self.ui.blocked(None);
                    self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::Unblocked);
                    self.push_tool_result(&call.id, "unblocked");
                }
                tools::TOOL_PROPOSE_SCOPE_CHANGE => {
                    let Some(args) = self.parse_or_report::<ProposeScopeChangeArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_propose_scope_change(&args);
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_MCP => {
                    let Some(args) = self.parse_or_report::<McpArgs>(call) else {
                        continue;
                    };
                    let label = match (args.action.as_str(), args.server.as_deref()) {
                        ("call", Some(s)) => {
                            format!("mcp call {s}.{}", args.tool.as_deref().unwrap_or("?"))
                        }
                        (a, Some(s)) => format!("mcp {a} {s}"),
                        (a, None) => format!("mcp {a}"),
                    };
                    self.ui.tool_use(&label);
                    let observation = self.run_mcp(&args).await;
                    self.push_tool_result(&call.id, &observation);
                }
                tools::TOOL_ASK_USER => {
                    let Some(args) = self.parse_or_report::<AskUserArgs>(call) else {
                        continue;
                    };
                    let answer = self
                        .ui
                        .ask_user(&args.question, &args.options.clone().unwrap_or_default());
                    self.push_tool_result(&call.id, &answer);
                }
                tools::TOOL_SUBAGENT => {
                    // Already executed in the concurrent pre-pass.
                    let result = sub_results
                        .get(&call.id)
                        .cloned()
                        .unwrap_or_else(|| "subagent error: no result produced".to_string());
                    self.push_tool_result(&call.id, &result);
                }
                other => {
                    // Via push_tool_result so it's LOGGED as well as pushed: an
                    // unlogged result leaves a permanent hole in transcript.jsonl
                    // (assistant tool call with no answer) that `--resume` can't
                    // repair once later turns bury it.
                    self.push_tool_result(&call.id, &format!("error: unknown tool {other}"));
                }
            }
        }
        Ok(None)
    }

    /// Record a tool error as an observation so the model can self-correct.
    fn tool_error(&mut self, id: &str, name: &str, err: &str) {
        let observation =
            format!("error: invalid arguments for `{name}`: {err}; please correct and retry");
        self.push_tool_result(id, &observation);
    }

    /// Parse a tool call's arguments, or record a tool error and return `None`
    /// (the caller `continue`s to the next call). Collapses the parse-or-bail
    /// boilerplate that every tool-dispatch arm would otherwise repeat.
    fn parse_or_report<T: serde::de::DeserializeOwned>(
        &mut self,
        call: &cowboy_core::model::ToolCall,
    ) -> Option<T> {
        match parse_args::<T>(&call.arguments) {
            Ok(a) => Some(a),
            Err(e) => {
                self.tool_error(&call.id, &call.name, &e.to_string());
                None
            }
        }
    }

    /// Run a structured file operation in the container, record the observation
    /// for the model, and log it. Returns (exit_code, helper output).
    /// Read a workspace-relative file from the host. The workspace is bind-
    /// mounted into the container, so the host sees exactly what the agent edits
    /// — letting us snapshot the before/after for a diff without a container
    /// round-trip. `None` if the path doesn't exist or isn't valid UTF-8.
    fn read_workspace_file(&self, path: &str) -> Option<String> {
        // Use the same hardened resolver as the in-container fileop: it rejects
        // absolute paths and `..` escapes (a lexical `starts_with` does NOT, so a
        // path like `../../etc/passwd` would otherwise read host files).
        let full = crate::cmd::fileop::resolve(self.root(), path).ok()?;
        std::fs::read_to_string(full).ok()
    }

    /// Compute a unified diff of a just-edited file (host-side) and report it to
    /// the UI for +/- rendering. Best-effort: skips binary/oversized changes.
    fn emit_file_diff(&mut self, path: &str, before: &str, after: &str) {
        if before == after {
            return;
        }
        // Cap the rendered diff so a huge file rewrite doesn't flood the pane;
        // the full change is still in the session log / on disk.
        const MAX_DIFF_LINES: usize = 200;
        let diff = unified_diff(path, before, after, MAX_DIFF_LINES);
        if !diff.is_empty() {
            self.ui.file_diff(path, &diff);
        }
    }

    async fn run_fileop(
        &mut self,
        call_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(i32, String)> {
        // `grep`/`ls` deliberately put their true totals last, so they are the ones
        // that must not lose their tail to the cap.
        let (code, output, observation) = self.fileop_observation(payload, Trim::Ends).await;
        self.push_tool_result(call_id, &observation);
        Ok((code, output))
    }

    /// Run a fileop and shape its observation, *without* recording it in the
    /// conversation. Split from [`Self::run_fileop`] for the read arm, which may
    /// replace the observation with a pointer to an identical earlier read before it
    /// lands in the context. Returns `(exit code, raw output, observation)`.
    async fn fileop_observation(
        &mut self,
        payload: &serde_json::Value,
        trim: Trim,
    ) -> (i32, String, String) {
        let outcome = self.runtime.fileop(&payload.to_string()).await;
        // A fileop can trigger container bring-up too (e.g. after an idle stop);
        // surface any status lines it queued, even though only after the fact.
        self.drain_runtime_status();
        let (result, output) = match outcome {
            Ok(v) => v,
            Err(e) => return (-1, String::new(), format!("error: {e}")),
        };
        let observation = if result.exit_code == 0 {
            output.clone()
        } else {
            format!("error: {}", output.trim())
        };
        // `push_tool_result` applies the same cap; truncating here as well keeps the
        // `[exit code: N]` prefix outside the truncated region.
        let cap = self.behavior.max_command_output_bytes;
        let observation = match trim {
            Trim::Head => truncate(&observation, cap),
            Trim::Ends => truncate_middle(&observation, cap),
            Trim::Read => {
                // Reserve room for the hint *and* for `truncate`'s own marker, which it
                // appends on top of the limit: appending the hint after cutting to the
                // full cap would push the result over, and `push_tool_result`'s
                // head-only backstop would then shear off the hint itself — the one
                // part that says the window is partial.
                const HINT_RESERVE: usize = 192;
                let cut = truncate(&observation, cap.saturating_sub(HINT_RESERVE));
                match read_continuation_hint(&cut).filter(|_| cut.len() < observation.len()) {
                    Some(hint) => format!("{cut}{hint}"),
                    None => cut,
                }
            }
        };
        (result.exit_code, output, observation)
    }

    /// Bookkeeping common to a successful `edit` or `write`: render the change, hand
    /// a bounded diff back to the model, and invalidate the verification record.
    /// Returns the observation to record.
    fn after_file_change(
        &mut self,
        path: &str,
        before: Option<String>,
        exit: i32,
        mut observation: String,
    ) -> String {
        if exit != 0 {
            return observation;
        }
        let after = self.read_workspace_file(path);
        // One read, two renderings: the UI gets a generous diff, the model a tight one.
        let before_str = before.as_deref().unwrap_or("");
        let after_str = after.as_deref().unwrap_or("");
        self.emit_file_diff(path, before_str, after_str);
        if let Some(note) = applied_change_note(path, before_str, after_str) {
            observation.push('\n');
            observation.push_str(&note);
        }
        // The agent has now seen this file's current content, so a later overwrite of
        // it is not blind.
        self.seen_files.note(path, &after);
        // The tree changed, so any earlier passing check no longer vouches for it.
        self.verification.note_edit();
        observation
    }

    /// Refuse a `write` that would destroy content the agent has not seen, naming
    /// what to do about it. `None` means go ahead.
    ///
    /// `write` is a full-file overwrite with no `old` to guard it, so unlike `edit` it
    /// cannot fail on a stale assumption — it silently wins. Two ways that loses work
    /// in this harness specifically: subagents are dispatched **in parallel into the
    /// same workspace**, so two workers editing one file is an ordinary occurrence;
    /// and a build, codegen step or formatter the agent itself started rewrites files
    /// under it. Requiring that the current bytes are the bytes the agent last
    /// observed turns both into a refusal it can recover from with one `read`, instead
    /// of a lost update nobody notices.
    ///
    /// Creating a new file is never blocked, and neither is overwriting a file the
    /// agent read or wrote and which has not changed since.
    fn stale_write_refusal(&self, path: &str, current: Option<&str>) -> Option<String> {
        let current = current?; // a new file — nothing to lose
        match self.seen_files.status(path, current) {
            SeenStatus::Match => None,
            SeenStatus::Unseen => Some(format!(
                "blocked: {path} already exists ({} bytes) and you have not read it in this \
                 session, so `write` would overwrite content you have not seen. `read` it \
                 first, then `write` (or `edit` the part you meant to change).",
                current.len()
            )),
            SeenStatus::Changed => Some(format!(
                "blocked: {path} has changed on disk since you last read it — something else \
                 (a parallel worker, a build, a formatter, the user) wrote to it, and this \
                 `write` would discard that. `read` it again and re-apply your change on top \
                 of what is there now; prefer `edit` so you only touch your part.",
            )),
        }
    }

    /// The timeout this `shell` call will get: its own, clamped to the host ceiling,
    /// else the session default. Enforcement is still the sandbox's — this only picks
    /// the bound to hand it, and the observation quotes the same number.
    fn shell_timeout(&self, args: &ShellArgs) -> u64 {
        match args.timeout_seconds {
            Some(t) => t.min(MAX_SHELL_TIMEOUT_SECONDS),
            None => self.behavior.command_timeout_seconds,
        }
    }

    /// The `proc` tool: background processes for the session.
    ///
    /// Host-handled rather than a CLI the agent shells out to, because the process has
    /// to be owned by something that outlives a single command. A `shell` call is a
    /// whole sandbox whose PID namespace is torn down when the command returns, so a
    /// backgrounded server there is dead before the next tool call — which is exactly
    /// what `cowboy proc start` used to do while reporting success. Here the worker
    /// owns it, so it lives as long as the session and is reaped with it.
    async fn run_proc(&mut self, args: &tools::ProcArgs) -> String {
        let action = args.action.trim().to_ascii_lowercase();
        if action == "list" {
            let running = self.runtime.running_processes();
            let mut out = if running.is_empty() {
                "no background processes running\n".to_string()
            } else {
                let mut s = String::from("running:\n");
                for name in &running {
                    s.push_str(&format!(
                        "- {name} (log: {})\n",
                        crate::sandbox::native::proc_log_path(name)
                    ));
                }
                s
            };
            if !self.processes.is_empty() {
                out.push_str("defined in agent.yaml (start by name, no `command` needed):\n");
                for (name, def) in &self.processes {
                    out.push_str(&format!("- {name}: `{}`\n", def.command));
                }
            }
            return out;
        }
        let Some(name) = args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            return format!("error: `name` is required for proc action {action:?}");
        };
        // A name becomes a filename and a registry key; keep it boring so it cannot
        // escape the log directory or collide with shell syntax.
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            || name.starts_with('.')
        {
            return format!(
                "error: process name {name:?} must be letters, digits, `-`, `_` or `.` \
                 (it names a log file)"
            );
        }
        match action.as_str() {
            "start" | "restart" => {
                if action == "restart" {
                    let _ = self.runtime.stop_process(name).await;
                }
                // A configured process supplies its own command and cwd; an ad-hoc one
                // must bring a command.
                let def = self.processes.get(name).cloned();
                let command = match args
                    .command
                    .as_deref()
                    .or(def.as_ref().map(|d| d.command.as_str()))
                {
                    Some(c) if !c.trim().is_empty() => c.to_string(),
                    _ => {
                        return format!(
                            "error: {name} is not defined in agent.yaml's `processes:`, so \
                             `command` is required"
                        )
                    }
                };
                let cwd = args
                    .cwd
                    .clone()
                    .or_else(|| def.as_ref().map(|d| d.cwd.clone()));
                match self
                    .runtime
                    .start_process(name, &command, cwd.as_deref())
                    .await
                {
                    Ok(()) => {
                        let log = crate::sandbox::native::proc_log_path(name);
                        self.ui
                            .notice(&format!("started background process {name}"));
                        format!(
                            "started {name}: `{command}`\nIt is running in the background and \
                             reachable on localhost from your `shell` commands. Output goes to \
                             {log} — give it a moment, then check `proc` logs (or wait for the \
                             port) before assuming it is up. It stops when the session ends.\n"
                        )
                    }
                    Err(e) => format!("error: could not start {name}: {e:#}"),
                }
            }
            "stop" => match self.runtime.stop_process(name).await {
                Ok(()) => format!("stopped {name}\n"),
                Err(e) => format!("error: {e:#}"),
            },
            "logs" => {
                let lines = args.lines.unwrap_or(80).clamp(1, 2000);
                let path = crate::sandbox::native::proc_log_path(name);
                match self.read_workspace_file(&path) {
                    Some(text) if !text.is_empty() => {
                        let all: Vec<&str> = text.lines().collect();
                        let from = all.len().saturating_sub(lines);
                        let body = all[from..].join("\n");
                        let head = if from > 0 {
                            format!(
                                "{path} (last {} of {} lines):\n",
                                all.len() - from,
                                all.len()
                            )
                        } else {
                            format!("{path} ({} lines):\n", all.len())
                        };
                        truncate_middle(
                            &format!("{head}{body}\n"),
                            self.behavior.max_command_output_bytes,
                        )
                    }
                    Some(_) => {
                        format!("{path} is empty — the process has produced no output yet\n")
                    }
                    None => format!(
                        "no log for {name} at {path} — it has not been started in this session\n"
                    ),
                }
            }
            other => format!(
                "error: unknown proc action {other:?}; use start, stop, restart, list or logs"
            ),
        }
    }

    /// Run a shell command with live streaming to the UI (interruptible via the
    /// turn's cancel token). Returns (exit, full output).
    async fn run_shell_streaming(&mut self, args: &ShellArgs) -> Result<(ExecResult, String)> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let timeout_secs = self.shell_timeout(args);
        let fut = self.runtime.exec_stream(
            &args.command,
            args.cwd.as_deref(),
            timeout_secs,
            self.cancel.clone(),
            tx,
        );
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                // Container bring-up progress (exec_stream may be (re)starting the
                // container — e.g. after an idle stop) surfaces as notices.
                Some(msg) = self.runtime_status.recv() => self.ui.notice(&msg),
                Some(chunk) = rx.recv() => self.ui.command_output(&chunk),
                res = &mut fut => {
                    while let Ok(msg) = self.runtime_status.try_recv() {
                        self.ui.notice(&msg);
                    }
                    while let Ok(chunk) = rx.try_recv() {
                        self.ui.command_output(&chunk);
                    }
                    return res;
                }
            }
        }
    }

    /// A handle for delivering user input **into the running turn**.
    ///
    /// The worker holds a clone: a message typed while the agent is working goes here
    /// rather than into the post-turn queue, and is picked up at the next iteration
    /// boundary. Explicitly deferred input (`Enqueue`) still goes to the queue.
    pub fn steer_sender(&self) -> tokio::sync::mpsc::UnboundedSender<String> {
        self.steer_tx.clone()
    }

    /// Publish the current job state to the UI. Called after anything that changes it,
    /// so a client renders from one level-triggered event rather than reconstructing the
    /// pane from a stream of edges.
    fn emit_jobs(&mut self) {
        let views = self.jobs.views();
        self.ui.jobs_changed(&views);
    }

    /// Whether the parent that dispatched this worker has disappeared.
    ///
    /// A worker's results go to one reader. If that reader is gone — the worker process
    /// was killed rather than asked to stop — everything from here on is spend with
    /// nowhere to land, so the worker stops itself. Always false for a top-level
    /// session, which has no parent to lose.
    fn orphaned(&self) -> bool {
        self.parent_pid.is_some_and(process_is_gone)
    }

    /// Fold any user input that arrived mid-turn into the conversation. Returns how
    /// many messages landed.
    fn drain_steering(&mut self) -> usize {
        let mut landed = 0;
        while let Ok(text) = self.steer_rx.try_recv() {
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            self.ui.steering(&text);
            self.ui.notice(&format!("↳ steering: {text}"));
            // A plain user message, marked so the model can tell mid-turn direction
            // from the original task — it is a correction to act on now, not a new
            // request to start from scratch.
            self.push_user_note(format!(
                "[the user says, while you are working] {text}\n\n\
                 Take this into account from here on. Do not restart what you have \
                 already done."
            ));
            landed += 1;
        }
        landed
    }

    /// A handle that stops every running subagent, usable while a turn is in flight.
    ///
    /// The worker holds a clone so "stop the subagents" (and session teardown) does not
    /// have to wait for `&mut` on the loop. Session-scoped jobs make this necessary:
    /// nothing else guarantees a child dies.
    pub fn job_stopper(&self) -> crate::agent::jobs::JobStopper {
        self.job_stopper.clone()
    }

    /// Stop every background job and settle the registry. Called on session end (and
    /// by an explicit "stop subagents"), before [`Self::shutdown`].
    ///
    /// Load-bearing rather than tidy: children are host processes that outlive a turn
    /// by design, so if this doesn't run they are only reaped when the worker exits —
    /// and not at all if it is killed.
    pub fn stop_all_jobs(&mut self) -> usize {
        // Fire the shared switch first: it reaches tasks that are still queued behind a
        // concurrency permit, which have no child to abort yet.
        self.job_stopper.stop_all();
        let stopped = self.jobs.stop_all();
        for id in &stopped {
            if let Some(dir) = self
                .logger
                .as_ref()
                .and_then(|l| crate::agent::jobctl::ControlDir::create(l.id(), id))
            {
                dir.cleanup();
            }
        }
        if !stopped.is_empty() {
            self.ui
                .notice(&format!("stopped {} background subagent(s)", stopped.len()));
        }
        self.emit_jobs();
        stopped.len()
    }

    /// How many background jobs are still running (for the UI and the worker).
    pub fn running_jobs(&self) -> usize {
        self.jobs.outstanding().len()
    }

    /// End-of-session teardown: stop managed processes, then the sandbox itself.
    ///
    /// The sandbox stop is explicit rather than left to `Drop`. `SessionSandbox` does
    /// release its namespaces on drop, but the thing being released here is the
    /// security boundary — an interception ruleset, a network namespace, a cgroup —
    /// and that should be relinquished at a point the caller chose and can bound with
    /// a timeout, not wherever the value happens to fall out of scope.
    pub async fn shutdown(&self) {
        let _ = self.runtime.stop_all_processes().await;
        self.runtime.stop().await;
    }

    /// Idle teardown: tear the sandbox down to free its resources. The next command
    /// brings it back (via the runtime's `ensure_running`). Used by the worker when a
    /// detached session sits idle past the configured timeout.
    pub async fn stop_container(&self) {
        self.runtime.stop().await;
    }

    /// The configured idle-container timeout (0 = disabled).
    pub fn idle_sandbox_timeout_seconds(&self) -> u64 {
        self.behavior.idle_sandbox_timeout_seconds
    }

    /// Plan every `subagent` call in this turn, announce it, and **start it in the
    /// background**. Returns call id → the result recorded for that call, which is a
    /// dispatch acknowledgement rather than the subagent's answer: results arrive
    /// later, injected at an iteration boundary by [`Self::drain_job_events`].
    ///
    /// This is the change that unwedges delegation. Awaiting the batch here meant the
    /// foreman made no model calls, ran no other tools and answered nobody until the
    /// slowest child exited. Now it keeps working, and `wait` is available for when it
    /// genuinely has nothing else to do.
    ///
    /// Parse / depth errors still become the result for that call, immediately.
    fn dispatch_subagents(
        &mut self,
        calls: &[cowboy_core::model::ToolCall],
    ) -> std::collections::HashMap<String, String> {
        let mut results: std::collections::HashMap<String, String> = Default::default();
        let sub_calls: Vec<&cowboy_core::model::ToolCall> = calls
            .iter()
            .filter(|c| c.name == tools::TOOL_SUBAGENT)
            .collect();
        if sub_calls.is_empty() {
            return results;
        }
        // Re-read per turn (not cached at construction) so a mid-session `cowboy crew`
        // edit takes effect on routing.
        let crew_cfg = cowboy_core::crew::load().ok().flatten();

        // Plan + announce sequentially (needs `&mut self`); collect runnable plans.
        let mut plans: Vec<(String, SubagentPlan)> = Vec::new();
        for call in &sub_calls {
            match parse_args::<SubagentArgs>(&call.arguments) {
                Ok(args) => match self.plan_subagent(&args, &crew_cfg) {
                    Ok(plan) => {
                        self.announce_subagent(&plan);
                        plans.push((call.id.clone(), plan));
                    }
                    Err(msg) => {
                        results.insert(call.id.clone(), msg);
                    }
                },
                Err(e) => {
                    results.insert(
                        call.id.clone(),
                        format!("error: invalid subagent args: {e}"),
                    );
                }
            }
        }
        if plans.is_empty() {
            return results;
        }
        let model_defs = load_model_defs(self.root());
        let foreman = crate::cmd::crew::foreman_model();
        let per_provider = crew_cfg
            .as_ref()
            .map(|c| c.delegation.max_parallel_per_provider)
            .unwrap_or(2) as usize;
        let max_parallel = crew_cfg
            .as_ref()
            .map(|c| c.delegation.max_parallel.max(1) as usize)
            .unwrap_or(4);
        // Announce true concurrency, not just the batch size: with a per-provider cap,
        // a batch of same-provider subagents runs a few at a time and the rest queue.
        // Saying "running N in parallel" when only 2 can run is misleading.
        if plans.len() > 1 {
            self.ui.notice(&concurrency_notice(
                &plans,
                per_provider,
                max_parallel,
                &model_defs,
                foreman.as_deref(),
            ));
        }

        for (call_id, plan) in plans {
            let label = plan
                .label
                .split(" → ")
                .next()
                .unwrap_or(&plan.label)
                .to_string();
            let (granted, ceiling) = plan.budget.unwrap_or((0, 0));
            let spec = crate::agent::jobs::JobSpec {
                id: plan.id.clone(),
                call_id: call_id.clone(),
                label: label.clone(),
                model: plan
                    .model
                    .clone()
                    .unwrap_or_else(|| "<default>".to_string()),
                task: plan.display_task.clone(),
                provider: provider_key(plan.model.as_deref(), &model_defs, foreman.as_deref()),
                granted,
                ceiling,
            };
            let job_id = plan.id.clone();
            let id_for_message = plan.id.clone();
            let model_disp = spec.model.clone();
            // Cloned out of `self` before the dispatch: the closure runs while the
            // registry is mutably borrowed, so it must not touch `self` at all.
            let tx = self.job_tx.clone();
            let fanout = self.fanout_sem.clone();
            let stop = self.job_stopper.token();
            self.jobs.dispatch(spec, move |_, provider_sem| {
                let handle = tokio::spawn(async move {
                    // Two throttles, acquired in a fixed order: the session-wide
                    // fan-out cap, then the per-provider one that keeps a burst of
                    // same-model workers off a provider's rate limit. Held for the
                    // child's whole life.
                    let _fanout = fanout.acquire_owned().await.ok();
                    let _provider = match provider_sem {
                        Some(s) => s.acquire_owned().await.ok(),
                        None => None,
                    };
                    let _ = tx.send(crate::agent::jobs::JobEvent::Started { id: job_id.clone() });
                    let started = std::time::Instant::now();
                    let routed = plan.routed.clone();
                    let sub_dir = plan.id.clone();
                    let watch = plan
                        .control_dir
                        .clone()
                        .map(|d| (d, plan.id.clone(), tx.clone()));
                    // Racing the stop switch, so "stop the subagents" reaches a running
                    // child even while the foreman's turn holds `&mut` on the loop.
                    // Dropping the exec future kills the child via `kill_on_drop`.
                    //
                    // The third branch is how a child's request for more turns reaches
                    // the foreman: the child writes it to its control directory and
                    // blocks, and this poll turns that file into a job event. Polling
                    // rather than signalling because the two ends are separate
                    // processes and the file *is* the message — a watch would add a
                    // dependency for a check that costs nothing at this interval.
                    let result = tokio::select! {
                        biased;
                        _ = stop.cancelled() => {
                            let _ = tx.send(crate::agent::jobs::JobEvent::Finished {
                                id: job_id,
                                ok: false,
                                result: format!(
                                    "[stopped] this subagent was stopped before it finished. \
                                     Whatever it completed is in its session directory \
                                     ({sub_dir}) — resume from that checkpoint rather than \
                                     redoing the work."
                                ),
                            });
                            return;
                        }
                        _ = watch_turn_requests(watch) => unreachable!("the watcher never ends"),
                        r = exec_subagent(plan) => r,
                    };
                    let status = classify_subagent_result(&result).to_string();
                    // Recorded here rather than on delivery: the crew history is an
                    // append to a file, and it should not depend on the foreman
                    // getting around to reading the result.
                    if let Some((category, effort, model, fell_back)) = routed {
                        cowboy_core::crew::record_outcome(&cowboy_core::crew::CrewOutcome {
                            ts_ms: now_ms(),
                            category,
                            effort,
                            model,
                            fell_back,
                            status: status.clone(),
                            duration_ms: started.elapsed().as_millis() as u64,
                        });
                    }
                    let _ = tx.send(crate::agent::jobs::JobEvent::Finished {
                        id: job_id,
                        ok: status == "complete",
                        result,
                    });
                });
                Box::new(handle.abort_handle())
            });
            results.insert(
                call_id,
                format!(
                    "dispatched: job `{id_for_message}` [{label}] on {model_disp}. It runs \
                     in the background — keep working. Its result will be delivered to \
                     you automatically when it finishes; use `jobs` to check on it, and \
                     `wait` when you have nothing else to do."
                ),
            );
        }
        self.emit_jobs();
        results
    }

    /// Non-blocking: fold in whatever running jobs have reported, then inject any
    /// undelivered news into the conversation.
    ///
    /// Called at the top of each iteration, which is the only safe place to add to the
    /// history: mid-batch would interleave with the tool results the provider expects
    /// to follow an assistant turn.
    fn drain_job_events(&mut self) {
        while let Ok(event) = self.job_rx.try_recv() {
            self.jobs.apply_event(event);
        }
        self.deliver_job_news();
    }

    /// Turn undelivered job news into conversation messages (and UI events).
    fn deliver_job_news(&mut self) {
        use crate::agent::jobs::JobNews;
        let news = self.jobs.drain_undelivered();
        if !news.is_empty() {
            self.emit_jobs();
        }
        for news in news {
            match news {
                JobNews::Started { id, label, model } => {
                    self.ui.subagent_started(&label, &model, &id);
                }
                JobNews::Finished {
                    id,
                    label,
                    ok,
                    result,
                } => {
                    self.ui.subagent_done(&label, ok, &id);
                    // Roll the finished subagent's spend into the session total so the
                    // UI reflects delegated work; subagents run as separate processes,
                    // so their cost would otherwise be invisible.
                    let usage = read_subagent_usage(self.root(), &id);
                    self.subagent_cost_usd += usage.cost_usd;
                    self.subagent_tokens_in += usage.tokens_in;
                    self.subagent_tokens_out += usage.tokens_out;
                    self.report_usage();
                    // The control channel is per-job; once it has finished there is
                    // nothing left to ask or answer.
                    if let Some(dir) = self
                        .logger
                        .as_ref()
                        .and_then(|l| crate::agent::jobctl::ControlDir::create(l.id(), &id))
                    {
                        dir.cleanup();
                    }
                    // A user message, not a tool result: the `subagent` call that
                    // started this job was already answered with its dispatch id, and a
                    // second result for a settled call id is a malformed conversation.
                    // Middle truncation, not head: a worker's result ends with its
                    // conclusion and handoff, which is the part the foreman needs.
                    let capped = truncate_middle(&result, self.behavior.max_command_output_bytes);
                    let body = format!("[subagent {label} · job {id}] finished:\n{capped}");
                    self.push_user_note(body);
                }
                JobNews::Question {
                    id,
                    label,
                    question,
                    options,
                    ..
                } => {
                    self.ui
                        .notice(&format!("⏸ subagent {label} ({id}) is asking a question"));
                    // The foreman is asked, not told: a subagent that hits a genuine
                    // ambiguity used to receive "" ("proceed") and guess, which surfaced
                    // as a confidently wrong result with no trace of the fork in the road.
                    let choices = if options.is_empty() {
                        String::new()
                    } else {
                        format!("\n\nIt suggested: {}", options.join(" · "))
                    };
                    let body = format!(
                        "[subagent {label} · job {id}] is blocked on a question and cannot \
                         continue until you answer:\n\n{question}{choices}\n\n\
                         Answer it with `job_reply` using `verdict: \"answer\"` and your \
                         reply in `instructions`. Answer from what you know about the \
                         overall task — that context is why it is asking you rather than \
                         guessing. If you cannot, say so: it will proceed on its own \
                         judgement, which is what it would have done anyway."
                    );
                    self.push_user_note(body);
                }
                JobNews::TurnRequest {
                    id,
                    label,
                    report,
                    requested,
                    used,
                    granted,
                    ceiling,
                    ..
                } => {
                    self.ui.notice(&format!(
                        "⏸ subagent {label} ({id}) is asking for {requested} more turns"
                    ));
                    let headroom = ceiling.saturating_sub(granted);
                    let body = format!(
                        "[subagent {label} · job {id}] has spent its turn grant \
                         ({used}/{granted}) and is asking for {requested} more. It is \
                         paused until you answer.\n\n{report}\n\n\
                         Decide with `job_reply`: `grant` (up to {headroom} more turns \
                         are available before its host ceiling of {ceiling}), `redirect` \
                         with instructions if it is going the wrong way, `wrap_up` to \
                         make it write up what it has now, or `stop` if the work is no \
                         longer wanted. Judge it on the measured evidence above, not on \
                         its own optimism. If you do not answer, it takes one small \
                         extension and then wraps up."
                    );
                    self.push_user_note(body);
                }
            }
        }
    }

    /// Append a synthetic user message (job news, steering, a directive) to the
    /// conversation and the session log.
    fn push_user_note(&mut self, body: String) {
        let msg = Message::user(body);
        if let Some(l) = &mut self.logger {
            l.log_message(&msg);
        }
        self.messages.push(msg);
    }

    /// Wait for a running job to report something, bounded and interruptible. Reports
    /// *why* it woke, because the caller's next move depends on it: news means "check
    /// whether we are done waiting", user input means "stop waiting and act".
    ///
    /// This is the only place the loop blocks on delegation, and it is reached only when
    /// the foreman asks (`wait`) or when it tried to finish with jobs still running.
    /// Cancellation is checked first so an interrupt is never swallowed by a long wait.
    async fn await_job_news(&mut self, timeout: std::time::Duration) -> Woke {
        if self.jobs.is_idle() {
            return Woke::TimedOut;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Woke::Cancelled,
                // The user typed something: stop waiting and act on it. Parking on a
                // subagent must never make the session unresponsive — that is the
                // defect this whole change exists to fix.
                Some(text) = self.steer_rx.recv() => {
                    self.ui.notice(&format!("↳ steering: {text}"));
                    self.push_user_note(format!(
                        "[the user says, while you are working] {text}\n\n\
                         Take this into account from here on. Do not restart what you have \
                         already done."
                    ));
                    return Woke::Steered;
                }
                _ = tokio::time::sleep_until(deadline) => return Woke::TimedOut,
                e = self.job_rx.recv() => e,
            };
            let Some(event) = event else {
                return Woke::TimedOut;
            };
            self.jobs.apply_event(event);
            // Drain anything else that landed in the same instant, so a batch that
            // finishes together is delivered together.
            while let Ok(e) = self.job_rx.try_recv() {
                self.jobs.apply_event(e);
            }
            let before = self.messages.len();
            self.deliver_job_news();
            if self.messages.len() > before {
                return Woke::News;
            }
            // A `Started` event is not news the foreman needs; keep waiting.
        }
    }

    /// Resolve a delegation into an executable plan: enforce the depth limit,
    /// route the model via the crew roster (category + effort), and build the
    /// worker brief. No side effects (so a batch can be planned then run
    /// concurrently). `Err` carries a message to return to the model as-is.
    fn plan_subagent(
        &self,
        args: &SubagentArgs,
        crew_cfg: &Option<cowboy_core::crew::CrewConfig>,
    ) -> std::result::Result<SubagentPlan, String> {
        use cowboy_core::crew;

        let max_depth = effective_max_depth(crew_cfg.as_ref());
        if self.subagent_depth >= max_depth {
            return Err(format!(
                "error: delegation depth limit ({max_depth}) reached; do this work directly"
            ));
        }
        let exe = self_exe().map_err(|e| format!("subagent error: {e}"))?;

        // The planner requests a KIND of work; Cowboy owns the model choice.
        let category = args
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(crew::GENERAL)
            .to_string();
        let effort = args
            .effort
            .as_deref()
            .and_then(crew::Effort::parse)
            .unwrap_or(crew::DEFAULT_EFFORT);
        // `<default>` roster slots (and the fallback) resolve to the foreman —
        // this process's own model (a routed COWBOY_MODEL, else the selection).
        let foreman = crate::cmd::crew::foreman_model().unwrap_or_default();
        let routed = crew_cfg
            .as_ref()
            .map(|c| c.resolve(&category, effort, &foreman));
        let temperature = crew_cfg.as_ref().and_then(|c| c.temperature_for(&category));
        // The child's iteration budget: an effort-scaled grant plus the ceiling it
        // can be granted up to. `max_total_iterations: 0` opts out of supervision,
        // and with no roster at all there is nothing to scale from — either way the
        // child falls back to `agent.max_iterations`, as it did before.
        let budget = crew_cfg.as_ref().and_then(|c| {
            let d = &c.delegation;
            (d.max_total_iterations > 0).then(|| (d.grant_for(effort), d.max_total_iterations))
        });

        // Worker brief: an optional adopted agent persona, then context, the task,
        // then the expected artifact.
        let mut task = String::new();
        let mut agent_name = None;
        if let Some(name) = args
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some(agent) = cowboy_core::agents::load(self.runtime.root(), name) {
                task.push_str(&format!(
                    "You are the `{}` agent.\n\n{}\n\n---\n\n",
                    agent.name, agent.instructions
                ));
                agent_name = Some(agent.name);
            } else {
                // Unknown agent: tell the worker to read the file itself (the
                // skill convention) rather than silently dropping the persona.
                task.push_str(&format!(
                    "Act as the `{name}` agent: read `.claude/agents/{name}.md` (or \
                     `.cowboy/agents/{name}.md`) and follow it.\n\n---\n\n"
                ));
                agent_name = Some(name.to_string());
            }
        }
        if let Some(ctx) = &args.context {
            if !ctx.is_empty() {
                task.push_str(ctx);
                task.push_str("\n\n");
            }
        }
        task.push_str(&args.task);
        if let Some(art) = args.expected_artifact.as_deref().filter(|s| !s.is_empty()) {
            task.push_str(&format!("\n\nExpected artifact: {art}"));
        }

        let who = agent_name
            .as_deref()
            .map(|a| format!("{a} "))
            .unwrap_or_default();
        let label = match &routed {
            Some(r) => format!("{who}{category}/{} → {}", effort.as_str(), r.model),
            None => format!("{who}{category}/{}", effort.as_str()),
        };
        let id = format!(
            "{}-sub{}",
            now_ms(),
            SUBAGENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // The channel this child will ask for more turns on. Created here, eagerly, so
        // the directory already exists when the child looks for it — and only when the
        // child actually has a budget to ask about. Keyed by *this* session's id, so
        // both ends derive the same path without passing one another a path to trust.
        let control_dir = budget.and_then(|_| {
            self.logger.as_ref().and_then(|l| {
                crate::agent::jobctl::ControlDir::create(l.id(), &id)
                    .map(|d| d.path().to_path_buf())
            })
        });
        Ok(SubagentPlan {
            exe,
            root: self.runtime.root().to_path_buf(),
            id,
            child_depth: self.subagent_depth + 1,
            task,
            display_task: args.task.clone(),
            label,
            model: routed.as_ref().map(|r| r.model.clone()),
            temperature,
            budget,
            control_dir,
            routed: routed.map(|r| (category, effort.as_str().to_string(), r.model, r.fell_back)),
        })
    }

    /// Surface a planned delegation to the UI + lifecycle log (needs `&mut self`,
    /// so it runs before the concurrent exec).
    fn announce_subagent(&mut self, plan: &SubagentPlan) {
        self.ui.notice(&format!(
            "↳ subagent [{}]: {}",
            plan.label, plan.display_task
        ));
        // Pane label is the category/effort part (the model is shown separately).
        let label = plan.label.split(" → ").next().unwrap_or(&plan.label);
        // Announce as *pending*: the per-provider throttle means a dispatched
        // subagent may wait for a permit before it actually runs. It flips to
        // running when it acquires one (emitted from the execution stream).
        self.ui.subagent_pending(
            label,
            plan.model.as_deref().unwrap_or("<default>"),
            &plan.id,
        );
        if let Some((category, effort, model, fell_back)) = &plan.routed {
            self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::SubagentRouted {
                category: category.clone(),
                effort: effort.clone(),
                model: model.clone(),
                fell_back: *fell_back,
            });
        }
    }

    /// Call the model, streaming deltas to the UI, racing cancellation.
    async fn call_model(&mut self) -> Result<ChatResponse> {
        // The one place every request passes through, and so the only place the
        // tool-call pairing invariant can be guaranteed rather than hoped for. A
        // repair here means something upstream trimmed across a turn boundary; that
        // has lost information either way, but it must not produce a conversation the
        // provider refuses — which would fail every later turn too, not just this one.
        let repaired = Self::enforce_tool_call_pairing(
            &mut self.messages,
            "not run: this turn was trimmed out of the conversation to fit the context window",
        );
        if repaired > 0 {
            tracing::warn!(
                repaired,
                "repaired {repaired} orphaned tool call(s)/result(s) before calling the model"
            );
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Delta>();
        // A truncation recovery asks for minimal reasoning for exactly this call.
        // Taken (not merely read) so it cannot leak into later turns, and resolved
        // before the borrow below: `with_minimal_reasoning` returns None for backends
        // with no such control, which just means the retry is the prompt alone.
        let low_effort = std::mem::take(&mut self.minimize_reasoning_next_turn)
            .then(|| self.model.with_minimal_reasoning())
            .flatten();
        let client = low_effort.as_deref().unwrap_or(self.model.as_ref());
        let fut = client.chat(&self.messages, &self.tools, Some(tx));
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    anyhow::bail!("interrupted");
                }
                Some(piece) = rx.recv() => {
                    emit_delta(self.ui, piece);
                }
                res = &mut fut => {
                    while let Ok(piece) = rx.try_recv() {
                        emit_delta(self.ui, piece);
                    }
                    self.ui.model_done();
                    return res.map_err(Into::into);
                }
            }
        }
    }
}

/// Route a streamed delta to the UI (answer text vs. dimmed reasoning). A free
/// function so it borrows only the UI, not all of `self` (the in-flight chat
/// future holds an immutable borrow of the loop). See `support` / `handlers`.
use cowboy_core::time::now_ms;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::jobctl::Verdict;
    use crate::agent::ui::AgentUi;
    use crate::sandbox::ExecResult;
    use crate::sandbox::{Sandbox, StatusRx, StatusTx};
    use cowboy_core::config::SecurityConfig;
    use cowboy_core::model::{ChatResponse, ToolCall};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Mutex;

    /// A model that returns a scripted sequence of responses.
    ///
    /// `low_effort_calls` counts calls made through the minimal-reasoning variant, so a
    /// test can assert the loop actually turned the knob rather than only asking nicely
    /// in the prompt. The queue is shared with the variant: it stands in for one
    /// endpoint, which is what the real client's clone is.
    #[derive(Clone)]
    struct ScriptedModel {
        responses: Arc<Mutex<std::collections::VecDeque<ChatResponse>>>,
        low_effort_calls: Arc<Mutex<usize>>,
        minimal: bool,
    }
    impl ScriptedModel {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                low_effort_calls: Arc::new(Mutex::new(0)),
                minimal: false,
            }
        }
    }
    #[async_trait::async_trait]
    impl ModelClient for ScriptedModel {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            deltas: Option<tokio::sync::mpsc::UnboundedSender<Delta>>,
        ) -> Result<ChatResponse, cowboy_core::Error> {
            if self.minimal {
                *self.low_effort_calls.lock().unwrap() += 1;
            }
            let r = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            if let (Some(tx), Some(c)) = (deltas, &r.content) {
                let _ = tx.send(Delta::Content(c.clone()));
            }
            Ok(r)
        }

        fn with_minimal_reasoning(&self) -> Option<Box<dyn ModelClient>> {
            if self.minimal {
                return None;
            }
            Some(Box::new(Self {
                minimal: true,
                ..self.clone()
            }))
        }
    }

    #[derive(Default)]
    struct RecordingUi {
        commands: Vec<String>,
        finals: Vec<String>,
        notices: Vec<String>,
        tool_uses: Vec<String>,
        costs: Vec<f64>,
        plans: Vec<Vec<(String, String)>>,
        blocked: Vec<Option<String>>,
        /// Questions the agent put to the user, so a test can assert on what the
        /// user would have been shown, not only on the outcome.
        asks: Vec<String>,
        /// The answer to give. `None` keeps the historical "yes".
        ask_answer: Option<String>,
    }
    impl AgentUi for RecordingUi {
        fn model_delta(&mut self, _text: &str) {}
        fn cost(&mut self, usd: f64) {
            self.costs.push(usd);
        }
        fn plan(&mut self, steps: &[(String, String)]) {
            self.plans.push(steps.to_vec());
        }
        fn blocked(&mut self, reason: Option<&str>) {
            self.blocked.push(reason.map(str::to_string));
        }
        fn command_start(&mut self, command: &str) {
            self.commands.push(command.to_string());
        }
        fn command_end(&mut self, _exit_code: i32, _output: &str) {}
        fn tool_use(&mut self, summary: &str) {
            self.tool_uses.push(summary.to_string());
        }
        fn final_message(&mut self, message: &str) {
            self.finals.push(message.to_string());
        }
        fn ask_user(&mut self, question: &str, _options: &[String]) -> String {
            self.asks.push(question.to_string());
            self.ask_answer.clone().unwrap_or_else(|| "yes".to_string())
        }
        fn notice(&mut self, msg: &str) {
            self.notices.push(msg.to_string());
        }
    }

    /// A [`Sandbox`] for the loop's own tests: records what it was asked to run and
    /// returns a scripted result.
    ///
    /// The loop is tested against neither a real sandbox nor a container mock, which
    /// is the whole reason the seam exists. A real sandbox would make every one of
    /// these tests depend on kernel features and cost a namespace each; a container
    /// mock made them depend on a runtime that no longer exists. What the loop needs
    /// from a sandbox is "run this, here is the output", and that is all this
    /// provides.
    struct FakeSandbox {
        root: PathBuf,
        session: String,
        /// What every command "prints", and the code it exits with.
        output: String,
        exit_code: i32,
        /// Commands in the order the loop asked for them, so a test can assert on
        /// what was run rather than only on what the UI showed.
        ran: Arc<Mutex<Vec<String>>>,
        status: Mutex<Option<StatusTx>>,
        /// When set, the output changes per call instead of being fixed.
        counting: bool,
        calls: std::sync::atomic::AtomicUsize,
        /// Background processes, as name → the command it was started with. Enough to
        /// test the `proc` tool's bookkeeping without a real namespace.
        procs: Mutex<std::collections::BTreeMap<String, String>>,
    }

    impl FakeSandbox {
        fn new() -> Self {
            let tmp = assert_fs::TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();
            // Leaked so the directory outlives the sandbox for the whole test.
            std::mem::forget(tmp);
            Self {
                root,
                session: "cowboy-test".into(),
                output: String::new(),
                exit_code: 0,
                ran: Arc::new(Mutex::new(Vec::new())),
                status: Mutex::new(None),
                counting: false,
                calls: std::sync::atomic::AtomicUsize::new(0),
                procs: Mutex::new(std::collections::BTreeMap::new()),
            }
        }

        /// Every command prints `output`.
        fn printing(output: &str) -> Self {
            Self {
                output: output.to_string(),
                ..Self::new()
            }
        }

        /// Every command fails with `code`.
        #[allow(dead_code)]
        fn failing(code: i32, output: &str) -> Self {
            Self {
                exit_code: code,
                output: output.to_string(),
                ..Self::new()
            }
        }

        /// Use `root` as the project root, for tests that inspect what the loop
        /// writes there (the per-worktree setup marker, for instance).
        fn at(root: PathBuf) -> Self {
            Self {
                root,
                ..Self::new()
            }
        }

        /// Each command prints something *different* (`attempt 0`, `attempt 1`, …),
        /// which is what distinguishes legitimate polling from a stuck loop.
        fn counting() -> Self {
            Self {
                counting: true,
                ..Self::new()
            }
        }

        /// A handle to the command log, cloneable so a test can read it after the
        /// sandbox has moved into the loop.
        fn log(&self) -> Arc<Mutex<Vec<String>>> {
            self.ran.clone()
        }

        /// Put a real file in the workspace. The file ops are the real ones, so a test
        /// that edits or reads something needs it to exist.
        fn with_file(self, rel: &str, content: &str) -> Self {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
            self
        }

        /// The workspace root, for asserting on what actually reached disk.
        fn root_path(&self) -> PathBuf {
            self.root.clone()
        }

        fn record(&self, command: &str) -> (ExecResult, String) {
            self.ran.lock().unwrap().push(command.to_string());
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let out = if self.counting {
                format!("attempt {n}")
            } else {
                self.output.clone()
            };
            (
                ExecResult {
                    exit_code: self.exit_code,
                },
                out,
            )
        }
    }

    #[async_trait::async_trait]
    impl Sandbox for FakeSandbox {
        fn root(&self) -> &Path {
            &self.root
        }
        fn session_name(&self) -> &str {
            &self.session
        }
        fn status_channel(&mut self) -> StatusRx {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *self.status.lock().unwrap() = Some(tx);
            rx
        }
        fn has_mise_config(&self) -> bool {
            false
        }
        async fn ensure_running(&self) -> Result<()> {
            Ok(())
        }
        async fn stop(&self) {}
        async fn exec_stream(
            &self,
            command: &str,
            _cwd: Option<&str>,
            _timeout_secs: u64,
            _cancel: tokio_util::sync::CancellationToken,
            chunks: StatusTx,
        ) -> Result<(ExecResult, String)> {
            let (result, out) = self.record(command);
            if !out.is_empty() {
                let _ = chunks.send(out.clone());
            }
            Ok((result, out))
        }
        async fn run_capture(
            &self,
            command: &str,
            _cwd: Option<&str>,
            _timeout_secs: u64,
        ) -> Result<(ExecResult, String)> {
            Ok(self.record(command))
        }
        async fn run(&self, argv: &[String]) -> Result<ExecResult> {
            Ok(self.record(&argv.join(" ")).0)
        }
        async fn shell(&self) -> Result<ExecResult> {
            Ok(ExecResult { exit_code: 0 })
        }
        async fn fileop(&self, payload: &str) -> Result<(ExecResult, String)> {
            self.ran.lock().unwrap().push(payload.to_string());
            // The real implementation, against this fake's root. A fake that
            // reimplemented read/edit/write would be a second version of exactly the
            // behaviour these tests are checking the loop against.
            Ok(match crate::cmd::fileop::apply(&self.root, payload) {
                Ok(out) => (ExecResult { exit_code: 0 }, out),
                Err(e) => (ExecResult { exit_code: 1 }, format!("Error: {e:#}")),
            })
        }
        async fn stop_all_processes(&self) -> Result<()> {
            self.procs.lock().unwrap().clear();
            Ok(())
        }

        async fn start_process(&self, name: &str, command: &str, _cwd: Option<&str>) -> Result<()> {
            let mut procs = self.procs.lock().unwrap();
            if procs.contains_key(name) {
                anyhow::bail!("process {name} is already running");
            }
            procs.insert(name.to_string(), command.to_string());
            Ok(())
        }

        async fn stop_process(&self, name: &str) -> Result<()> {
            if self.procs.lock().unwrap().remove(name).is_none() {
                anyhow::bail!("process {name} is not running");
            }
            Ok(())
        }

        fn running_processes(&self) -> Vec<String> {
            self.procs.lock().unwrap().keys().cloned().collect()
        }
        fn add_grant(
            &self,
            _path: &Path,
            _read_only: bool,
            _persistence: crate::sandbox::grants::Persistence,
        ) -> Result<()> {
            Ok(())
        }
        fn granted_paths(&self) -> Vec<(PathBuf, bool)> {
            Vec::new()
        }
    }

    fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[tokio::test]
    async fn runs_shell_then_final() {
        let sandbox = FakeSandbox::printing("file1\nfile2\n");
        let ran = sandbox.log();

        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("inspecting".into()),
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"ls"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done; tests pass"}"#)],
            },
        ]);

        let behavior = cowboy_core::config::AgentBehavior::default();
        let cancel = CancellationToken::new();
        let mut ui = RecordingUi::default();
        let mut agent =
            AgentLoop::new(Box::new(model), sandbox, behavior, 200_000, cancel, &mut ui);
        let final_msg = agent.run("list the files then finish").await.unwrap();

        assert_eq!(final_msg.as_deref(), Some("done; tests pass"));
        assert_eq!(ui.commands, vec!["ls"]);
        assert_eq!(ui.finals, vec!["done; tests pass"]);
        // And the sandbox really was asked to run it — the UI showing a command is
        // not the same as the command reaching the sandbox.
        assert_eq!(*ran.lock().unwrap(), vec!["ls".to_string()]);
    }

    /// A model that re-runs one inspection with only cosmetic changes (a different
    /// counter, an added `echo`, reflowed whitespace) — and gets trivially
    /// different output each time — is escalated through the churn ladder (nudge →
    /// strong intervention → abort) well before `max_iterations`, even though the
    /// strict same-call/same-result guard exempts it as "polling". Regression test
    /// for a real session that burned all 100 iterations this way.
    #[tokio::test]
    async fn cosmetic_churn_escalates_then_aborts_before_max_iterations() {
        // `counting()` returns a *different* output per call, so `last_obs_changed`
        // is true every turn — the polling exemption that used to let this run.
        let sandbox = FakeSandbox::counting();
        let ran = sandbox.log();

        // The same core inspection, each turn with a distinct cosmetic tweak the old
        // guard treated as a different command. Generate well past CHURN_ABORT_AT so
        // the whole ladder is exercised; every variant folds to the same normalized
        // signature but has a different raw command.
        let core = r#"git show HEAD -- ranch.rs | grep -E \"test|dead-sid\""#;
        let responses: Vec<ChatResponse> = (0..25)
            .map(|i| {
                // A unique cosmetic tail per turn: a trailing echo carrying the turn
                // number (narration the normalizer strips) plus alternating counters.
                let tail = if i % 2 == 0 {
                    format!(" | wc -l; echo \\\"step {i}\\\"")
                } else {
                    format!(" ; echo \\\"probe {i}\\\" 2>&1")
                };
                ChatResponse {
                    truncated: false,
                    usage: None,
                    reasoning: None,
                    content: None,
                    tool_calls: vec![tool_call(
                        &format!("c{i}"),
                        "shell",
                        &format!(r#"{{"command":"cd /workspace && {core}{tail}"}}"#),
                    )],
                }
            })
            .collect();
        let total = responses.len();

        let behavior = cowboy_core::config::AgentBehavior::default(); // max_iterations 100
        let cancel = CancellationToken::new();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            sandbox,
            behavior,
            200_000,
            cancel,
            &mut ui,
        );
        let out = agent.run("review the ranch commit").await.unwrap();

        // The intervention must actually reach the model as a forceful directive to
        // do something different — not merely a UI notice. It is pushed as a tool
        // result, so it lands in the conversation the model sees on its next turn.
        // Capture this before dropping `agent` (which holds the &mut borrow of `ui`).
        let strong_in_history = agent.messages.iter().any(|m| {
            m.role == Role::Tool
                && m.content.contains("[loop guard — STOP]")
                && m.content.contains("Do NOT run this command")
        });
        let ran_count = ran.lock().unwrap().len();
        drop(agent);

        // Stopped without a final answer …
        assert_eq!(out, None);
        // … having climbed the full escalation ladder, in order: a gentle nudge, a
        // STRONG intervention that orders a different action, then the hard abort.
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("nudging a change of approach")),
            "expected the nudge stage, got: {:?}",
            ui.notices
        );
        assert!(
            ui.notices.iter().any(|n| n.contains("STRONG intervention")),
            "expected the strong-intervention stage, got: {:?}",
            ui.notices
        );
        assert!(
            ui.notices.iter().any(|n| n.contains("ending the turn")),
            "expected the hard-abort stage, got: {:?}",
            ui.notices
        );
        assert!(
            strong_in_history,
            "the strong 'do something different' directive must be delivered to the model"
        );
        // Never the iteration cap.
        assert!(
            !ui.notices
                .iter()
                .any(|n| n.contains("reached max_iterations")),
            "should have stopped on churn, not the iteration cap"
        );
        // … and it stopped well before exhausting the scripted turns.
        assert!(
            ran_count < total,
            "guard should stop before running all {total} variants"
        );
    }

    #[test]
    fn setup_hash_changes_with_commands() {
        assert_eq!(setup_hash(&["a".into()]), setup_hash(&["a".into()]));
        assert_ne!(setup_hash(&["a".into()]), setup_hash(&["b".into()]));
        assert_ne!(
            setup_hash(&["a".into()]),
            setup_hash(&["a".into(), "b".into()])
        );
    }

    /// A configured `setup` command runs on the first session in a worktree (and
    /// writes the marker); a second session over the same worktree skips it.
    #[tokio::test]
    async fn setup_commands_run_once_per_worktree() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let build = |root: &std::path::Path| {
            let behavior = cowboy_core::config::AgentBehavior {
                setup: vec!["pnpm install".into()],
                ..Default::default()
            };
            (FakeSandbox::at(root.to_path_buf()), behavior)
        };

        let marker = root
            .join(".cowboy")
            .join("sessions")
            .join(".worktree-setup");

        // First session: runs the setup command + writes the marker.
        let (rt1, b1) = build(&root);
        let mut ui1 = RecordingUi::default();
        let mut a1 = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            rt1,
            b1,
            200_000,
            CancellationToken::new(),
            &mut ui1,
        );
        a1.run_session_setup().await;
        assert!(marker.exists(), "marker written after successful setup");
        assert!(
            ui1.commands.iter().any(|c| c == "pnpm install"),
            "setup command ran on the first session, got {:?}",
            ui1.commands
        );

        // Second session over the same worktree: marker present → skip.
        let (rt2, b2) = build(&root);
        let mut ui2 = RecordingUi::default();
        let mut a2 = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            rt2,
            b2,
            200_000,
            CancellationToken::new(),
            &mut ui2,
        );
        a2.run_session_setup().await;
        assert!(
            !ui2.commands.iter().any(|c| c == "pnpm install"),
            "setup must be skipped when the worktree marker is present, got {:?}",
            ui2.commands
        );
    }

    #[tokio::test]
    async fn provider_usage_drives_cost_with_cache_discount() {
        // The provider reports 1000 prompt tokens (800 cached) + 100 completions;
        // pricing is $3/Mtok in, $0.30/Mtok cached, $15/Mtok out. Expected:
        // 200*3/1e6 + 800*0.30/1e6 + 100*15/1e6 = 0.0006 + 0.00024 + 0.0015.
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: Some(cowboy_core::model::Usage {
                prompt_tokens: 1000,
                completion_tokens: 100,
                cached_prompt_tokens: 800,
            }),
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call("1", "final", r#"{"message":"done"}"#)],
        }]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_model_pricing(ModelPricing {
            input: Some(3.0),
            output: Some(15.0),
            cached_input: Some(0.30),
        });
        agent.run("go").await.unwrap();

        let expected = 200.0 * 3.0 / 1e6 + 800.0 * 0.30 / 1e6 + 100.0 * 15.0 / 1e6;
        let got = *ui.costs.last().expect("priced model reports cost");
        assert!(
            (got - expected).abs() < 1e-12,
            "cache-aware cost: got {got}, want {expected}"
        );
    }

    #[tokio::test]
    async fn provider_usage_without_a_cache_price_bills_full_input_rate() {
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: Some(cowboy_core::model::Usage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                cached_prompt_tokens: 800,
            }),
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call("1", "final", r#"{"message":"done"}"#)],
        }]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_pricing(Some(3.0), Some(15.0)); // no cached rate → full input price
        agent.run("go").await.unwrap();

        let got = *ui.costs.last().expect("priced model reports cost");
        let expected = 1000.0 * 3.0 / 1e6;
        assert!(
            (got - expected).abs() < 1e-12,
            "no cache discount configured: got {got}, want {expected}"
        );
        // …and it says so, because the figure is knowably too high.
        //
        // This is the bug that made a real session read $18.70 against a $2.50 bill: 99%
        // of the prompt tokens were cache reads billed at the full input rate, and
        // nothing on screen hinted at why. Full price stays the fallback — it never
        // understates spend, which is the right bias for a cost display, and the
        // discount varies 3%–19% between models so guessing one would trade a knowable
        // overstatement for an unknowable error.
        let warned = ui
            .notices
            .iter()
            .find(|n| n.contains("cost is overstated"))
            .unwrap_or_else(|| panic!("expected a cache-price warning, got {:?}", ui.notices));
        assert!(
            warned.contains("80%"),
            "should name the cached share: {warned}"
        );
        assert!(
            warned.contains("cached_input_cost_per_mtok"),
            "should name the fix: {warned}"
        );
    }

    /// The warning stays quiet when there is nothing to warn about.
    #[tokio::test]
    async fn a_configured_cache_price_produces_no_warning() {
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: Some(cowboy_core::model::Usage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                cached_prompt_tokens: 800,
            }),
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call("1", "final", r#"{"message":"done"}"#)],
        }]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_model_pricing(ModelPricing {
            input: Some(3.0),
            output: Some(15.0),
            cached_input: Some(0.3),
        });
        agent.run("go").await.unwrap();
        assert!(
            !ui.notices.iter().any(|n| n.contains("cost is overstated")),
            "priced correctly, so nothing to warn about: {:?}",
            ui.notices
        );
    }

    #[tokio::test]
    async fn estimate_is_abandoned_once_provider_usage_arrives() {
        // First response carries no usage (estimated); the second does. The
        // estimated tokens from turn one must not linger in the cost basis —
        // once the provider reports, only counted tokens are billed.
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("a long estimated answer that the local tokenizer counts".into()),
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"ls"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: Some(cowboy_core::model::Usage {
                    prompt_tokens: 500,
                    completion_tokens: 10,
                    cached_prompt_tokens: 0,
                }),
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::printing("file1\n"),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_pricing(Some(3.0), Some(15.0));
        agent.run("go").await.unwrap();

        let got = *ui.costs.last().expect("priced model reports cost");
        let expected = 500.0 * 3.0 / 1e6 + 10.0 * 15.0 / 1e6;
        assert!(
            (got - expected).abs() < 1e-12,
            "only the provider-counted turn is billed: got {got}, want {expected}"
        );
    }

    #[tokio::test]
    async fn stops_when_token_budget_reached_and_reports_cost() {
        // The model keeps asking for shell (never finals); only the budget stops it.
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("working".into()),
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"ls"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("still working".into()),
                tool_calls: vec![tool_call("2", "shell", r#"{"command":"ls"}"#)],
            },
        ]);

        // token_budget of 1 trips on the second iteration (after the first turn's
        // tokens are accounted), before another model call is made.
        let behavior = cowboy_core::config::AgentBehavior {
            token_budget: 1,
            ..cowboy_core::config::AgentBehavior::default()
        };
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            behavior,
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_pricing(Some(3.0), Some(15.0)); // priced → cost is reported
        let out = agent.run("go").await.unwrap();

        assert_eq!(out, None, "the budget stops the run with no final answer");
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("token budget reached")),
            "expected a budget-stop notice, got {:?}",
            ui.notices
        );
        assert!(
            ui.costs.last().copied().unwrap_or(0.0) > 0.0,
            "a priced model should report a running cost"
        );
    }

    #[test]
    fn read_subagent_usage_takes_last_cost_and_tokens_from_journal() {
        use cowboy_core::daemonproto::UiEventMsg;
        let tmp = assert_fs::TempDir::new().unwrap();
        let root = tmp.path();
        let id = "sub-123";
        let dir = crate::session::session_dir(root, id);
        std::fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("events.jsonl");
        // Interleave unrelated events with several Cost/Tokens updates; the helper
        // must return the LAST of each (the child's combined running total).
        let events = [
            UiEventMsg::Tokens {
                input: 10,
                output: 2,
            },
            UiEventMsg::Cost(0.01),
            UiEventMsg::Notice("working".into()),
            UiEventMsg::Tokens {
                input: 100,
                output: 40,
            },
            UiEventMsg::Cost(0.25),
            UiEventMsg::Final("done".into()),
        ];
        let lines: Vec<String> = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        std::fs::write(&journal, lines.join("\n")).unwrap();

        let usage = read_subagent_usage(root, id);
        assert!((usage.cost_usd - 0.25).abs() < 1e-9, "last Cost wins");
        assert_eq!(usage.tokens_in, 100);
        assert_eq!(usage.tokens_out, 40);

        // A subagent with no journal (e.g. unpriced / never started) → zeros.
        let none = read_subagent_usage(root, "missing");
        assert_eq!(none.cost_usd, 0.0);
        assert_eq!((none.tokens_in, none.tokens_out), (0, 0));
    }

    #[test]
    fn provider_key_groups_same_provider_models_together() {
        use cowboy_core::config::ModelsConfig;
        let yaml = "default: a\nmodels:\n  \
                    a: { provider: fireworks, model: minimax-m3 }\n  \
                    b: { provider: fireworks, model: other }\n  \
                    c: { provider: openai, model: gpt }\n";
        let defs = serde_yaml_ng::from_str::<ModelsConfig>(yaml)
            .unwrap()
            .models;
        // Two different model NAMES on the same provider share a throttle key.
        assert_eq!(provider_key(Some("a"), &defs, None), "fireworks");
        assert_eq!(
            provider_key(Some("a"), &defs, None),
            provider_key(Some("b"), &defs, None),
        );
        // Different provider → different key.
        assert_ne!(
            provider_key(Some("a"), &defs, None),
            provider_key(Some("c"), &defs, None),
        );
        // Unknown model keys on its own name (still groups identical models).
        assert_eq!(provider_key(Some("zzz"), &defs, None), "zzz");
        // Roster-less worker keys on the foreman's provider, else a sentinel.
        assert_eq!(provider_key(None, &defs, Some("c")), "openai");
        assert_eq!(provider_key(None, &defs, None), "<foreman>");
    }

    #[tokio::test]
    async fn plan_tool_records_steps_and_normalizes_status() {
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "plan",
                    r#"{"steps":[{"step":"scope","status":"done"},
                                {"step":"build","status":"doing"},
                                {"step":"test"}]}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);

        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("go").await.unwrap();

        let plan = ui.plans.last().expect("a plan should have been emitted");
        assert_eq!(
            plan,
            &vec![
                ("scope".to_string(), "done".to_string()),
                ("build".to_string(), "in_progress".to_string()), // "doing" normalized
                ("test".to_string(), "pending".to_string()),      // missing status defaults
            ]
        );
    }

    #[test]
    fn with_memory_context_appends_to_system_message() {
        let model = ScriptedModel::new(vec![]);
        let mut ui = RecordingUi::default();
        let agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_memory_context("INDEX: build-uses-just".into());
        // Injected into the always-kept system message (never pruned).
        assert!(agent.messages[0].content.starts_with("You are Cowboy"));
        assert!(agent.messages[0].content.contains("INDEX: build-uses-just"));
        // Empty context is a no-op.
        let mut ui2 = RecordingUi::default();
        let agent2 = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui2,
        )
        .with_memory_context("   ".into());
        assert_eq!(agent2.messages.len(), 1);
    }

    #[tokio::test]
    async fn artifact_tool_publishes_to_the_session_store() {
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "artifact",
                    r##"{"action":"publish","kind":"contract","title":"API Contract",
                        "content":"# API\nGET /things\n","summary":"billing API"}"##,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);

        let runtime = FakeSandbox::new();
        let root = runtime.root().to_path_buf();
        let logger = crate::session::SessionLogger::create(&root).unwrap();
        let session_dir = logger.dir().to_path_buf();

        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        agent.run("go").await.unwrap();

        let arts = cowboy_core::artifact::list_in(&session_dir);
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].title, "API Contract");
        assert_eq!(arts[0].kind, cowboy_core::artifact::ArtifactKind::Contract);
        let (_, body) = cowboy_core::artifact::get_in(&session_dir, &arts[0].id).unwrap();
        assert!(body.contains("GET /things"));
    }

    #[tokio::test]
    async fn decision_tool_records_the_answer() {
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "decision",
                    r#"{"question":"UUIDs or sequential?","options":["uuid","sequential"]}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);

        let runtime = FakeSandbox::new();
        let root = runtime.root().to_path_buf();
        let logger = crate::session::SessionLogger::create(&root).unwrap();
        let session_dir = logger.dir().to_path_buf();

        let mut ui = RecordingUi::default(); // ask_user returns "yes"
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        agent.run("go").await.unwrap();

        let decisions = cowboy_core::decision::list_in(&session_dir);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].question, "UUIDs or sequential?");
        assert_eq!(decisions[0].selected.as_deref(), Some("yes"));
        // Recorded as a DecisionRecord artifact + lifecycle event.
        assert!(cowboy_core::artifact::list_in(&session_dir)
            .iter()
            .any(|a| a.kind == cowboy_core::artifact::ArtifactKind::DecisionRecord));
        assert!(cowboy_core::lifecycle::read_in(&session_dir)
            .iter()
            .any(|r| matches!(
                r.event,
                cowboy_core::lifecycle::LifecycleEvent::DecisionRecorded { .. }
            )));
    }

    #[tokio::test]
    async fn blocked_then_unblock_reports_and_logs() {
        use cowboy_core::lifecycle::{read_in, LifecycleEvent};

        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "blocked",
                    r#"{"reason":"need the API contract","waiting_on":["schema"]}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "unblock", "{}")],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("3", "final", r#"{"message":"done"}"#)],
            },
        ]);

        let runtime = FakeSandbox::new();
        let root = runtime.root().to_path_buf();
        let logger = crate::session::SessionLogger::create(&root).unwrap();
        let session_dir = logger.dir().to_path_buf();

        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        agent.run("go").await.unwrap();

        assert_eq!(
            ui.blocked,
            vec![Some("need the API contract".to_string()), None]
        );
        let events: Vec<_> = read_in(&session_dir).into_iter().map(|r| r.event).collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, LifecycleEvent::Blocked { reason, .. } if reason == "need the API contract")));
        assert!(events
            .iter()
            .any(|e| matches!(e, LifecycleEvent::Unblocked)));
    }

    #[tokio::test]
    async fn lifecycle_events_recorded_in_order() {
        use cowboy_core::lifecycle::{read_in, LifecycleEvent};

        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "plan",
                    r#"{"steps":[{"step":"build","status":"in_progress"}]}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "2",
                    "artifact",
                    r#"{"action":"publish","kind":"summary","title":"notes","content":"x"}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("3", "final", r#"{"message":"done"}"#)],
            },
        ]);

        let runtime = FakeSandbox::new();
        let root = runtime.root().to_path_buf();
        let logger = crate::session::SessionLogger::create(&root).unwrap();
        let session_dir = logger.dir().to_path_buf();

        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        agent.run("go").await.unwrap();

        let kinds: Vec<_> = read_in(&session_dir).into_iter().map(|r| r.event).collect();
        assert_eq!(kinds.first(), Some(&LifecycleEvent::SessionStarted));
        assert!(kinds
            .iter()
            .any(|e| matches!(e, LifecycleEvent::PlanStepStarted { step } if step == "build")));
        assert!(kinds
            .iter()
            .any(|e| matches!(e, LifecycleEvent::ArtifactPublished { .. })));
        assert!(matches!(
            kinds.last(),
            Some(LifecycleEvent::SessionCompleted { .. })
        ));
    }

    #[tokio::test]
    async fn handoff_tool_writes_handoff_md_and_artifact() {
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "handoff",
                    r#"{"goal":"add billing schema","status":"complete",
                        "contracts":"published schema-contract.md","next_steps":"wire the API"}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);

        let runtime = FakeSandbox::new();
        let root = runtime.root().to_path_buf();
        let logger = crate::session::SessionLogger::create(&root).unwrap();
        let session_dir = logger.dir().to_path_buf();

        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        agent.run("go").await.unwrap();

        let md = std::fs::read_to_string(session_dir.join("handoff.md")).unwrap();
        assert!(md.contains("## Goal\nadd billing schema"));
        assert!(md.contains("## Next steps\nwire the API"));
        // Registered as a Handoff artifact too.
        let arts = cowboy_core::artifact::list_in(&session_dir);
        assert!(arts
            .iter()
            .any(|a| a.kind == cowboy_core::artifact::ArtifactKind::Handoff));
    }

    #[tokio::test]
    async fn loop_guard_aborts_repeated_identical_action() {
        // Model keeps issuing the SAME shell call (same name+args; ids differ).
        let m = ScriptedModel::new(vec![]);
        {
            let mut q = m.responses.lock().unwrap();
            for i in 0..12 {
                q.push_back(ChatResponse {
                    truncated: false,
                    usage: None,
                    reasoning: None,
                    content: None,
                    tool_calls: vec![tool_call(
                        &i.to_string(),
                        "shell",
                        r#"{"command":"grep -rn x ."}"#,
                    )],
                });
            }
        }
        let behavior = cowboy_core::config::AgentBehavior::default(); // max_iterations 100
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(m),
            FakeSandbox::new(),
            behavior,
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("loop on a grep").await.unwrap();
        assert!(res.is_none());
        // Aborted by the loop guard, not run to max_iterations.
        assert!(
            ui.notices.iter().any(|n| n.contains("loop detected")),
            "notices: {:?}",
            ui.notices
        );
        // Only the first few identical commands ran before the guard kicked in.
        assert!(
            ui.commands.len() <= 3,
            "ran {} commands (guard should stop execution)",
            ui.commands.len()
        );
    }

    /// Polling — the same command with *changing* output — is progress, not a
    /// loop. The old guard keyed on the call alone and aborted these runs.
    #[tokio::test]
    async fn loop_guard_allows_polling_with_changing_output() {
        // Each identical poll returns a DIFFERENT observation (a health check going
        // from refused → starting → ok).
        let m = ScriptedModel::new(vec![]);
        {
            let mut q = m.responses.lock().unwrap();
            for i in 0..10 {
                q.push_back(ChatResponse {
                    truncated: false,
                    usage: None,
                    reasoning: None,
                    content: None,
                    tool_calls: vec![tool_call(
                        &i.to_string(),
                        "shell",
                        r#"{"command":"curl -s localhost:8080/health"}"#,
                    )],
                });
            }
            // Then it finishes normally.
            q.push_back(ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("f", "final", r#"{"message":"server is up"}"#)],
            });
        }
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(m),
            // Each identical poll must return a DIFFERENT observation (a health check
            // going refused → starting → ok); that difference is exactly what tells
            // polling apart from a stuck loop.
            FakeSandbox::counting(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("wait for the server").await.unwrap();
        drop(agent);
        assert_eq!(res.as_deref(), Some("server is up"));
        assert!(
            !ui.notices.iter().any(|n| n.contains("loop detected")),
            "polling must not trip the loop guard: {:?}",
            ui.notices
        );
        assert_eq!(ui.commands.len(), 10, "every poll should have run");
    }

    /// Every tool call in an assistant turn must end up with a result, including
    /// the `final` call itself and anything batched after it — a conversation with
    /// an unanswered tool call is rejected by strict providers on the NEXT turn.
    #[tokio::test]
    async fn final_answers_its_own_call_and_any_batched_after_it() {
        let m = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![
                tool_call("f", "final", r#"{"message":"done"}"#),
                tool_call("x", "read", r#"{"path":"never-read.txt"}"#),
            ],
        }]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(m),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("do it").await.unwrap();
        assert_eq!(res.as_deref(), Some("done"));

        let answered: std::collections::HashSet<&str> = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert!(answered.contains("f"), "the `final` call must be answered");
        assert!(
            answered.contains("x"),
            "a call batched after `final` never runs but must still be answered"
        );
    }

    /// The wrap-up directive must be enforced, not merely requested.
    ///
    /// Regression test for a real loss: a subagent that had spent 70 turns without
    /// writing anything answered "stop investigating and report" with fourteen more
    /// `grep`s, was stopped at the ceiling, and its entire investigation was
    /// unrecoverable. A worker in wrap-up must not be able to keep digging.
    #[tokio::test]
    async fn wrapping_up_refuses_investigation_and_leaves_only_reporting_tools() {
        let m = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![
                tool_call("g", "grep", r#"{"pattern":"MCP-0"}"#),
                tool_call("s", "shell", r#"{"command":"sed -n '1,40p' SPEC.md"}"#),
                tool_call("r", "read", r#"{"path":"SPEC.md"}"#),
                tool_call("e", "edit", r#"{"path":"a","old":"x","new":"y"}"#),
            ],
        }]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(m),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.enter_wrap_up();

        // The surface the model is offered no longer contains the tools it must stop
        // using, and still contains the one it has to use.
        let offered: Vec<&str> = agent.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(offered.contains(&"final"), "no way to report: {offered:?}");
        for gone in ["shell", "read", "grep", "ls", "edit", "write", "subagent"] {
            assert!(
                !offered.contains(&gone),
                "{gone} is still offered during wrap-up: {offered:?}"
            );
        }

        let _ = agent.run("review the MCP surface").await;
        let answered: Vec<String> = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .collect();
        drop(agent);

        // And if it calls one anyway — which it will, because the history is full of
        // earlier `shell` calls — every one is refused, and none of them ran.
        assert!(ui.commands.is_empty(), "a command ran during wrap-up");
        assert_eq!(answered.len(), 4, "every call answered: {answered:?}");
        assert!(
            answered
                .iter()
                .all(|c| c.starts_with("blocked: you are out of turns")),
            "all four must be refused: {answered:?}"
        );
        assert!(
            answered[0].contains("Call `final` now"),
            "the refusal must say what to do instead: {:?}",
            answered[0]
        );
    }

    /// The gate must not deadlock a foreman: `final` already refuses while delegated
    /// work is in flight, so a foreman in wrap-up needs the means to collect it.
    #[tokio::test]
    async fn wrapping_up_still_allows_collecting_delegated_work() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.enter_wrap_up();
        for needed in ["final", "artifact", "handoff"] {
            assert!(tools::allowed_when_wrapping_up(needed), "{needed}");
        }
        // Not offered unless delegation is available, but never denied *by the
        // wrap-up gate* — otherwise `final`'s in-flight refusal and this gate would
        // between them leave the foreman no legal move.
        for collecting in ["jobs", "wait", "job_reply"] {
            assert!(
                tools::allowed_when_wrapping_up(collecting),
                "{collecting} must survive wrap-up or a foreman deadlocks"
            );
        }
    }

    /// Plan mode is HOST-enforced: while planning, the agent must not be able to
    /// mutate the workspace via `shell`, nor escape the gate by delegating to a
    /// subagent (which is not itself in plan mode).
    #[tokio::test]
    async fn plan_mode_blocks_shell_and_subagent_not_just_edits() {
        // The gate must stop the command before it ever reaches the container.
        let m = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![
                tool_call("s", "shell", r#"{"command":"rm -rf src"}"#),
                tool_call("d", "subagent", r#"{"task":"apply the refactor"}"#),
            ],
        }]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(m),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.planning = true;
        let _ = agent.run("plan a refactor").await;
        let blocked: Vec<String> = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .collect();
        drop(agent);

        assert!(ui.commands.is_empty(), "no command may run while planning");
        assert_eq!(blocked.len(), 2, "both calls answered: {blocked:?}");
        assert!(
            blocked.iter().all(|c| c.starts_with("blocked: plan mode")),
            "shell and subagent must both be refused: {blocked:?}"
        );
    }

    /// A one-shot session has a single user message (the task). Pruning must never
    /// remove it — an agent that loses its task drifts for the rest of the run.
    #[test]
    fn pruning_preserves_the_task_message() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent
            .messages
            .push(Message::user("THE TASK: migrate the db"));
        for i in 0..60 {
            agent.messages.push(Message::new(
                Role::Assistant,
                format!("some long intermediate step number {i} with plenty of words"),
            ));
        }
        agent.drop_oldest(40); // tiny budget: forces heavy pruning
        assert_eq!(agent.messages[0].role, Role::System, "system kept");
        assert_eq!(
            agent.messages[1].content,
            "THE TASK: migrate the db",
            "the task must survive pruning; messages: {:?}",
            agent
                .messages
                .iter()
                .map(|m| &m.content)
                .collect::<Vec<_>>()
        );
    }

    /// A model the provider no longer serves (404 model_not_found) must reroute to
    /// the fallback and finish, not kill the session. This is the failure that made
    /// a whole crew review collapse: the roster named a retired model id, and crew
    /// `fell_back` is a routing-time flag that never re-evaluates at runtime.
    #[tokio::test]
    async fn unavailable_model_reroutes_to_the_fallback_and_continues() {
        /// Always fails as if the provider retired this model.
        struct GoneModel;
        #[async_trait::async_trait]
        impl ModelClient for GoneModel {
            async fn chat(
                &self,
                _m: &[Message],
                _t: &[ToolDef],
                _d: Option<tokio::sync::mpsc::UnboundedSender<Delta>>,
            ) -> Result<ChatResponse, cowboy_core::Error> {
                Err(cowboy_core::Error::ModelUnavailable(
                    "chat request failed (404 Not Found): model_not_found".into(),
                ))
            }
        }
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(GoneModel),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_model_fallback(
            "backup".into(),
            Box::new(|_name| {
                // The healthy fallback answers immediately.
                let m = ScriptedModel::new(vec![ChatResponse {
                    truncated: false,
                    usage: None,
                    reasoning: None,
                    content: None,
                    tool_calls: vec![tool_call("f", "final", r#"{"message":"rescued"}"#)],
                }]);
                Ok((
                    Box::new(m) as Box<dyn ModelClient>,
                    200_000,
                    ModelPricing::default(),
                ))
            }),
        );
        let res = agent.run("do the work").await.unwrap();
        drop(agent);

        assert_eq!(
            res.as_deref(),
            Some("rescued"),
            "the turn should complete on the fallback model"
        );
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("falling back to `backup`")),
            "the reroute must be surfaced, not silent: {:?}",
            ui.notices
        );
    }

    #[test]
    fn message_tokens_counts_reasoning_because_it_is_sent_back() {
        // Reasoning is round-tripped to the provider on every later request, so it
        // consumes context and is billed — it must be counted.
        let mut m = Message::new(Role::Assistant, "short answer");
        let plain = AgentLoop::message_tokens(&m);
        m.reasoning = Some("a very long chain of thought ".repeat(50));
        let with_reasoning = AgentLoop::message_tokens(&m);
        assert!(
            with_reasoning > plain + 100,
            "reasoning must be counted ({plain} → {with_reasoning})"
        );
    }

    #[test]
    fn with_history_inserts_after_system_in_order() {
        let history = vec![
            Message::user("earlier task"),
            Message::new(Role::Assistant, "earlier answer"),
        ];
        let mut ui = RecordingUi::default();
        let agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_history(history);
        // [system, user(earlier), assistant(earlier)] — system stays first.
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(agent.messages[0].role, Role::System);
        assert_eq!(agent.messages[1].role, Role::User);
        assert_eq!(agent.messages[1].content, "earlier task");
        assert_eq!(agent.messages[2].role, Role::Assistant);
    }

    #[tokio::test]
    /// The gate is host-measured: a session that edited a file and never ran the
    /// project's check is refused, told exactly what to run, and accepted once the
    /// check actually exits 0.
    async fn final_is_refused_until_the_projects_check_passes() {
        let sandbox = FakeSandbox::printing("ok\n").with_file("main.rs", "foo\n");
        let model = ScriptedModel::new(vec![
            // 1. Edit a file.
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "edit",
                    r#"{"path":"main.rs","old":"foo","new":"bar"}"#,
                )],
            },
            // 2. Claim done without checking — must be refused.
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"all good"}"#)],
            },
            // 3. Run the check.
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("3", "shell", r#"{"command":"cargo test"}"#)],
            },
            // 4. Now finishing is allowed.
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("4", "final", r#"{"message":"done, tests pass"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let commands =
            std::collections::BTreeMap::from([("test".to_string(), "cargo test".to_string())]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_project_commands(&commands, vec!["cargo test".to_string()]);

        let final_msg = agent.run("edit then finish").await.unwrap();
        assert_eq!(final_msg.as_deref(), Some("done, tests pass"));
        // The first `final` was refused, naming the command to run.
        let refusal = agent
            .messages
            .iter()
            .map(|m| m.content.clone())
            .find(|c| c.contains("blocked: this session changed files"))
            .expect("the unverified `final` should have been refused");
        assert!(
            refusal.contains("cargo test"),
            "must name the check: {refusal}"
        );
        // And the check really ran.
        assert!(ui.commands.iter().any(|c| c == "cargo test"));
    }

    /// A session that changed nothing has nothing to verify, so the gate must stay
    /// out of the way — otherwise every question and code review would be refused.
    #[tokio::test]
    async fn a_read_only_session_is_never_gated() {
        let sandbox = FakeSandbox::printing("     1\tfn main() {}\n");
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "read", r#"{"path":"main.rs"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"it is fine"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_project_commands(&Default::default(), vec!["cargo test".to_string()]);

        assert_eq!(
            agent.run("review this").await.unwrap().as_deref(),
            Some("it is fine"),
            "a session with no edits must finish without running checks"
        );
    }

    /// Quality pressure must never wedge a session: a check that cannot pass (broken
    /// toolchain, no network) is refused a bounded number of times and then yields,
    /// mirroring the outstanding-subagent gate.
    #[tokio::test]
    async fn the_verification_gate_yields_rather_than_wedging() {
        // The edit must succeed (so there is something to verify); the point of this
        // test is that the agent never runs the check and insists on finishing.
        let sandbox = FakeSandbox::printing("ok\n").with_file("main.rs", "foo\n");
        let mut script = vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call(
                "1",
                "edit",
                r#"{"path":"main.rs","old":"foo","new":"bar"}"#,
            )],
        }];
        // Insist on finishing more times than the refusal budget allows.
        for i in 0..6 {
            script.push(ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &format!("f{i}"),
                    "final",
                    r#"{"message":"cannot run the tests here"}"#,
                )],
            });
        }
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(script)),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_project_commands(&Default::default(), vec!["cargo test".to_string()]);

        assert_eq!(
            agent.run("edit then insist").await.unwrap().as_deref(),
            Some("cannot run the tests here"),
            "the gate must yield after its refusal budget"
        );
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("finishing with unverified edits")),
            "yielding must be said out loud, not silent: {:?}",
            ui.notices
        );
    }

    /// A failing check must not count as verification, and must invalidate an earlier
    /// pass of the same command.
    #[test]
    fn verification_only_counts_a_passing_run_against_the_current_tree() {
        let mut v = Verification::new(vec!["cargo test".to_string()]);
        assert!(
            !v.has_unverified_edits(),
            "no edits yet, so nothing to verify"
        );

        v.note_edit();
        assert!(v.has_unverified_edits());

        // A failing run is not evidence.
        v.note_command("cargo test", 1);
        assert!(v.has_unverified_edits());

        // A passing run is, and a wrapper around it still counts.
        v.note_command("cd . && cargo test 2>&1 | tail -20", 0);
        assert!(!v.has_unverified_edits());

        // A later edit invalidates it again.
        v.note_edit();
        assert!(v.has_unverified_edits());

        // A pass followed by a failure is not verified.
        v.note_command("cargo test", 0);
        assert!(!v.has_unverified_edits());
        v.note_command("cargo test", 1);
        assert!(v.has_unverified_edits());

        // An unrelated command is ignored entirely.
        let mut v = Verification::new(vec!["cargo test".to_string()]);
        v.note_edit();
        v.note_command("cargo build", 0);
        assert!(
            v.has_unverified_edits(),
            "a different command must not satisfy the requirement"
        );
        assert_eq!(v.outstanding(), vec!["cargo test"]);
    }

    /// With no `verify` configured there is no gate at all — the default for every
    /// existing project.
    #[test]
    fn verification_is_inert_by_default() {
        let mut v = Verification::default();
        assert!(!v.is_enabled());
        v.note_edit();
        assert!(
            !v.has_unverified_edits(),
            "an unconfigured project must never be gated"
        );
    }

    #[tokio::test]
    async fn runs_edit_via_fileop_then_final() {
        let sandbox = FakeSandbox::printing("").with_file("main.rs", "let x = foo;\n");
        let fileops = sandbox.log();
        let root = sandbox.root_path();
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "edit",
                    r#"{"path":"main.rs","old":"foo","new":"bar"}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let final_msg = agent.run("edit then finish").await.unwrap();
        assert_eq!(final_msg.as_deref(), Some("done"));
        // The UI showed the helper's status line for the edit.
        assert_eq!(
            ui.tool_uses,
            vec!["edited main.rs: 1 edit applied, 1 replacement"]
        );
        // And the edit really went through the structured file-op path, carrying the
        // op and the path — not through a shell command — and reached the file.
        let sent = fileops.lock().unwrap().join("\n");
        assert!(
            sent.contains("\"op\":\"edit\"") && sent.contains("main.rs"),
            "the edit should reach the file-op helper: {sent}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("main.rs")).unwrap(),
            "let x = bar;\n"
        );
    }

    /// A full-file `write` over a file the agent never read is refused: there is no
    /// `old` to guard it, so without this it silently wins over whatever is there.
    #[tokio::test]
    async fn a_write_over_an_unread_file_is_refused_until_it_is_read() {
        let sandbox = FakeSandbox::printing("").with_file("cfg.toml", "keep = true\n");
        let root = sandbox.root_path();
        let model = ScriptedModel::new(vec![
            // 1. Blind overwrite — refused.
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "write",
                    r#"{"path":"cfg.toml","content":"mine = 1\n"}"#,
                )],
            },
            // 2. Read it…
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "read", r#"{"path":"cfg.toml"}"#)],
            },
            // 3. …and now the same write is allowed.
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "3",
                    "write",
                    r#"{"path":"cfg.toml","content":"mine = 1\n"}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("4", "final", r#"{"message":"done"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("overwrite it").await.unwrap();

        let refusal = agent
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content.contains("blocked"))
            .map(|m| m.content.clone())
            .expect("the blind write should have been refused");
        assert!(refusal.contains("have not read it"), "{refusal}");
        assert!(
            refusal.contains("`read` it first"),
            "must say how: {refusal}"
        );
        // The refusal is recoverable: after the read, the write landed.
        assert_eq!(
            std::fs::read_to_string(root.join("cfg.toml")).unwrap(),
            "mine = 1\n"
        );
    }

    /// And a file that changed *since* the agent read it is refused too — the case
    /// that matters when parallel subagents share one workspace.
    #[tokio::test]
    async fn a_write_over_a_file_that_changed_since_the_read_is_refused() {
        let sandbox = FakeSandbox::printing("").with_file("shared.rs", "// original\n");
        let root = sandbox.root_path();
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "read", r#"{"path":"shared.rs"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"read it"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        // Stand in for the other worker: the loop has read the file, and now something
        // else writes to it before an overwrite arrives.
        agent.run("read then overwrite").await.unwrap();
        let theirs = "// someone else's work\n";
        std::fs::write(root.join("shared.rs"), theirs).unwrap();
        let refusal = agent
            .stale_write_refusal("shared.rs", Some(theirs))
            .expect("a file that changed since the read must be refused");
        assert!(refusal.contains("changed on disk"), "{refusal}");
        assert!(refusal.contains("prefer `edit`"), "{refusal}");

        // Unchanged since the read goes straight through, and so does creating a file
        // that does not exist yet — the guard must only bite on a real lost update.
        assert!(agent
            .stale_write_refusal("shared.rs", Some("// original\n"))
            .is_none());
        assert!(agent.stale_write_refusal("brand-new.rs", None).is_none());
    }

    /// An applied edit reports the diff back to the model, so it can see where the
    /// change landed without spending a turn re-reading the file.
    #[tokio::test]
    async fn an_applied_edit_reports_its_diff_to_the_model() {
        let sandbox =
            FakeSandbox::printing("").with_file("m.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "edit",
                    r#"{"path":"m.rs","old":"fn b() {}","new":"fn beta() {}"}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("rename b").await.unwrap();

        let result = agent
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content.contains("edited m.rs"))
            .map(|m| m.content.clone())
            .expect("the edit result");
        assert!(result.contains("applied change:"), "{result}");
        assert!(result.contains("-fn b() {}"), "{result}");
        assert!(result.contains("+fn beta() {}"), "{result}");
        // Surrounding context is the point — it is what confirms placement.
        assert!(result.contains(" fn a() {}"), "{result}");
    }

    /// A timed-out command must not reach the model as a bare `124`: it is
    /// indistinguishable from a real failing status, and the two useful reactions are
    /// not deducible from it.
    #[tokio::test]
    async fn a_timed_out_command_is_explained_with_both_ways_out() {
        let sandbox = FakeSandbox::failing(crate::sandbox::EXIT_TIMEOUT, "listening on :3000\n");
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"npm run dev"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior {
                command_timeout_seconds: 42,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("start the server").await.unwrap();

        let obs = agent
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content.contains("exit code"))
            .map(|m| m.content.clone())
            .expect("the shell result");
        assert!(obs.contains("timed out"), "{obs}");
        assert!(
            obs.contains("42s"),
            "must quote the timeout that fired: {obs}"
        );
        assert!(obs.contains("timeout_seconds"), "the first way out: {obs}");
        assert!(obs.contains("`proc` tool"), "the second way out: {obs}");
        // And every command reports how long it took, timeout or not.
        assert!(obs.contains("exit code: 124 · "), "{obs}");
    }

    /// The `proc` tool is the answer to "run a server, then talk to it": starting one
    /// registers it, `list` and `logs` can see it, and `stop` ends it.
    #[tokio::test]
    async fn the_proc_tool_starts_lists_and_stops_a_background_process() {
        let sandbox = FakeSandbox::printing("");
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let proc = |action: &str, name: Option<&str>, command: Option<&str>| tools::ProcArgs {
            action: action.into(),
            name: name.map(str::to_string),
            command: command.map(str::to_string),
            cwd: None,
            lines: None,
        };

        let out = agent
            .run_proc(&proc("start", Some("web"), Some("npm run dev")))
            .await;
        assert!(out.contains("started web"), "{out}");
        assert!(out.contains(".cowboy/proc/web.log"), "names the log: {out}");

        let listed = agent.run_proc(&proc("list", None, None)).await;
        assert!(listed.contains("web"), "{listed}");

        // An ad-hoc process with no command is refused rather than started empty.
        let bad = agent.run_proc(&proc("start", Some("other"), None)).await;
        assert!(bad.contains("`command` is required"), "{bad}");

        // A name that would escape the log directory is refused.
        let evil = agent
            .run_proc(&proc("start", Some("../../etc/x"), Some("true")))
            .await;
        assert!(evil.contains("must be letters"), "{evil}");

        let stopped = agent.run_proc(&proc("stop", Some("web"), None)).await;
        assert!(stopped.contains("stopped web"), "{stopped}");
        assert!(agent
            .run_proc(&proc("list", None, None))
            .await
            .contains("no background"));
    }

    /// `auto_start` was a config field nothing read. Now the session honours it once,
    /// before the agent's first turn.
    #[tokio::test]
    async fn auto_start_processes_are_running_before_the_first_turn() {
        let sandbox = FakeSandbox::printing("");
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call("1", "final", r#"{"message":"done"}"#)],
        }]);
        let mut ui = RecordingUi::default();
        let processes = std::collections::BTreeMap::from([
            (
                "web".to_string(),
                cowboy_core::config::ProcessDef {
                    command: "npm run dev".into(),
                    cwd: "/workspace".into(),
                    auto_start: true,
                },
            ),
            (
                "worker".to_string(),
                cowboy_core::config::ProcessDef {
                    command: "cargo run --bin worker".into(),
                    cwd: "/workspace".into(),
                    auto_start: false,
                },
            ),
        ]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_processes(processes);
        agent.run("do something").await.unwrap();

        assert_eq!(agent.runtime.running_processes(), vec!["web".to_string()]);
        // The declared processes are named in the prompt, so `proc start web` needs no
        // command.
        assert!(agent
            .messages
            .first()
            .is_some_and(|m| m.content.contains("worker: `cargo run --bin worker`")));
        drop(agent);
        assert!(
            ui.notices.iter().any(|n| n.contains("auto_start")),
            "and it is reported: {:?}",
            ui.notices
        );
    }

    /// `grep`/`ls` put their true totals last, so their observation must be truncated
    /// from the middle — head-only truncation drops exactly the line that says how
    /// much was not shown.
    #[tokio::test]
    async fn a_capped_grep_result_keeps_the_line_that_says_how_much_was_missed() {
        let sandbox = FakeSandbox::printing("").with_file(
            "big.txt",
            &(0..4000)
                .map(|i| format!("needle {i}\n"))
                .collect::<String>(),
        );
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "grep",
                    r#"{"pattern":"needle","max_results":2000}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior {
                // Small enough that the result is certainly capped.
                max_command_output_bytes: 4000,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("find the needles").await.unwrap();

        let obs = agent
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content.contains("needle 0"))
            .map(|m| m.content.clone())
            .expect("the grep result");
        assert!(obs.contains("bytes elided"), "must be middle-cut: {obs}");
        assert!(
            obs.contains("4000 matches"),
            "the true total must survive the cap: {obs}"
        );
    }

    /// A `read` whose window is cut by the byte cap has to say where it got to: the
    /// hint fileop puts at the end goes over the cliff with everything else.
    #[tokio::test]
    async fn a_truncated_read_says_which_line_to_continue_from() {
        let sandbox = FakeSandbox::printing("").with_file(
            "long.rs",
            &(0..3000).map(|i| format!("line {i}\n")).collect::<String>(),
        );
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "read", r#"{"path":"long.rs"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior {
                max_command_output_bytes: 2000,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("read it").await.unwrap();

        let obs = agent
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content.contains("line 0"))
            .map(|m| m.content.clone())
            .expect("the read result");
        assert!(obs.contains("output cap cut this read at line"), "{obs}");
        assert!(obs.contains("continue with offset="), "{obs}");
    }

    #[tokio::test]
    async fn plan_mode_blocks_edits_until_approved() {
        // The sandbox records everything it was asked to run, and the assertion at
        // the end is that it was asked for *nothing* — so this checks the gate
        // actually prevents the mutation rather than merely discouraging it.
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "edit",
                    r#"{"path":"main.rs","old":"a","new":"b"}"#,
                )],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"here is the plan"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.set_planning(true);
        let out = agent.run("plan it").await.unwrap();
        assert_eq!(out.as_deref(), Some("here is the plan"));
        // The agent got a plan-mode refusal observation instead of editing.
        let blocked = agent
            .messages
            .iter()
            .any(|m| m.content.contains("blocked: plan mode"));
        assert!(blocked, "edit should be refused with a plan-mode message");
        // No edit ran (no tool_use surfaced; the fileop mock was never called).
        assert!(ui.tool_uses.is_empty(), "no edit should run in plan mode");
    }

    #[tokio::test]
    async fn stops_at_max_iterations() {
        // Model always asks for another shell command -> never finishes.
        let looping = ScriptedModel::new(vec![]);
        // Empty queue returns default (no tool calls) -> would stop early; instead
        // script many shell calls to exercise the cap.
        {
            let mut q = looping.responses.lock().unwrap();
            for i in 0..10 {
                q.push_back(ChatResponse {
                    truncated: false,
                    usage: None,
                    reasoning: None,
                    content: None,
                    tool_calls: vec![tool_call(
                        &i.to_string(),
                        "shell",
                        r#"{"command":"echo hi"}"#,
                    )],
                });
            }
        }
        let behavior = cowboy_core::config::AgentBehavior {
            max_iterations: 3,
            ..Default::default()
        };
        // Declines the continue prompt, so this still measures the cap itself. The ask is
        // covered by `the_foreman_asks_the_user_before_giving_up_on_its_budget`.
        let mut ui = RecordingUi {
            ask_answer: Some("no".into()),
            ..Default::default()
        };
        let mut agent = AgentLoop::new(
            Box::new(looping),
            FakeSandbox::new(),
            behavior,
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("loop forever").await.unwrap();
        assert!(res.is_none());
        assert!(ui
            .notices
            .iter()
            .any(|n| n.contains("reached the iteration budget")));
        assert_eq!(ui.commands.len(), 3);
    }

    /// A model that never finishes, for the budget-extension tests.
    fn never_finishes(turns: usize) -> ScriptedModel {
        // Each turn runs a *different* command. Repeating one trips the loop/churn
        // guards, which abort the run — so the test would stop after a few turns
        // regardless of the answer, measuring the wrong thing. Distinct words rather
        // than `cmd-1`/`cmd-2`, because `normalize_shell_command` strips trailing
        // counters precisely so cosmetic variants collapse to one signature.
        const WORDS: [&str; 24] = [
            "alpha", "bravo", "charlie", "delta", "eddy", "foxtrot", "golf", "hotel", "india",
            "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
            "sierra", "tango", "uniform", "victor", "whiskey", "xray",
        ];
        let m = ScriptedModel::new(vec![]);
        {
            let mut q = m.responses.lock().unwrap();
            for i in 0..turns {
                let w = WORDS[i % WORDS.len()];
                let suffix = "z".repeat(i / WORDS.len());
                q.push_back(ChatResponse {
                    truncated: false,
                    usage: None,
                    reasoning: None,
                    content: None,
                    tool_calls: vec![tool_call(
                        &i.to_string(),
                        "shell",
                        &format!(r#"{{"command":"cat /tmp/{w}{suffix}"}}"#),
                    )],
                });
            }
        }
        m
    }

    fn budget_agent<'a>(
        model: ScriptedModel,
        ui: &'a mut RecordingUi,
        max_iterations: u32,
    ) -> AgentLoop<'a> {
        AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior {
                max_iterations,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            ui,
        )
    }

    /// The foreman asks rather than silently stopping.
    ///
    /// A delegated worker reports to its foreman and can be granted more turns; the
    /// foreman's equivalent is the person. Before this it just printed "reached the
    /// iteration budget" and ended the turn — recoverable (any message continues, with the
    /// conversation intact) but nothing said so, so a paused session looked like a
    /// finished one. Observed on a real review, where the answer was "keep going".
    #[tokio::test]
    async fn the_foreman_asks_the_user_before_giving_up_on_its_budget() {
        let mut ui = RecordingUi {
            ask_answer: Some("yes".into()),
            ..Default::default()
        };
        let mut agent = budget_agent(never_finishes(20), &mut ui, 3);
        let _ = agent.run("keep working").await.unwrap();

        assert!(
            ui.asks.iter().any(|q| q.contains("Keep going?")),
            "the user should be asked, not just told: {:?}",
            ui.asks
        );
        assert!(
            ui.notices.iter().any(|n| n.contains("continuing with")),
            "a yes should visibly extend: {:?}",
            ui.notices
        );
        // It really kept working rather than only saying so.
        assert!(
            ui.commands.len() > 3,
            "expected more than the initial 3 turns, got {}",
            ui.commands.len()
        );
    }

    #[tokio::test]
    async fn declining_ends_the_turn_and_says_how_to_resume() {
        let mut ui = RecordingUi {
            ask_answer: Some("no".into()),
            ..Default::default()
        };
        let mut agent = budget_agent(never_finishes(20), &mut ui, 3);
        let _ = agent.run("keep working").await.unwrap();

        assert_eq!(ui.commands.len(), 3, "a no must not extend anything");
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("Send a message to continue")),
            "the way back has to be stated, since that is the whole gap: {:?}",
            ui.notices
        );
    }

    /// Silence is not consent.
    ///
    /// `ask_user` returns an empty string when nobody *can* answer — a piped run, or no
    /// attached client. Reading that as yes would let an unattended session extend itself
    /// indefinitely, which is the opposite of what the cap is for.
    #[tokio::test]
    async fn a_run_with_nobody_to_ask_never_extends_itself() {
        let mut ui = RecordingUi {
            ask_answer: Some(String::new()),
            ..Default::default()
        };
        let mut agent = budget_agent(never_finishes(20), &mut ui, 3);
        let _ = agent.run("keep working").await.unwrap();
        assert_eq!(ui.commands.len(), 3, "an unanswered ask must stop the turn");
    }

    /// Yes is bounded: one message cannot become an indefinite run.
    #[tokio::test]
    async fn user_extensions_stop_at_the_cap() {
        let mut ui = RecordingUi {
            ask_answer: Some("yes".into()),
            ..Default::default()
        };
        // One turn per grant makes the arithmetic exact: 1 initial + MAX_USER_EXTENSIONS.
        let budget = 1 + MAX_USER_EXTENSIONS;
        let mut agent = budget_agent(never_finishes(budget as usize * 4), &mut ui, 1);
        let _ = agent.run("keep working").await.unwrap();

        assert_eq!(
            ui.commands.len(),
            budget as usize,
            "should stop after {MAX_USER_EXTENSIONS} extensions"
        );
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains(&format!("extended {MAX_USER_EXTENSIONS}×"))),
            "the cap should say why it stopped: {:?}",
            ui.notices
        );
    }

    #[test]
    fn only_an_explicit_yes_continues() {
        for yes in ["y", "yes", "YES", " ok ", "sure", "continue", "keep going"] {
            assert!(is_affirmative(yes), "{yes:?} should continue");
        }
        // Empty is what `ask_user` returns with nobody to answer — never a yes.
        for no in ["", "  ", "n", "no", "stop", "later", "maybe", "y e s"] {
            assert!(!is_affirmative(no), "{no:?} must not continue");
        }
    }

    #[tokio::test]
    async fn an_unchanged_file_is_not_read_into_the_context_twice() {
        // The loop reads the same path three times. The read is performed every time
        // (that is what proves it is unchanged), but only the first copy of the
        // contents reaches the conversation; the rest are answered with a pointer to
        // the step that has it. This is the cheap half of the fix for a worker that
        // spends its whole budget re-reading files.
        let responses: Vec<ChatResponse> = (0..3)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &i.to_string(),
                    "read",
                    r#"{"path":"src/main.rs"}"#,
                )],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::printing("").with_file("src/main.rs", "FILE CONTENTS HERE\n"),
            cowboy_core::config::AgentBehavior {
                max_iterations: 3,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("read it repeatedly").await.unwrap();

        let results: Vec<&str> = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(results.len(), 3, "every call still gets a result");
        assert!(
            results[0].contains("FILE CONTENTS HERE"),
            "the first read must deliver the file: {:?}",
            results[0]
        );
        for r in &results[1..] {
            assert!(
                !r.contains("FILE CONTENTS HERE"),
                "an unchanged re-read must not spend context again: {r:?}"
            );
            assert!(r.contains("not re-read"), "got: {r:?}");
            assert!(
                r.contains("step 1"),
                "should point at the first read: {r:?}"
            );
        }
    }

    #[tokio::test]
    async fn going_in_circles_gets_a_directive_naming_the_measured_evidence() {
        // Six identical reads with a stall window of 3: the host notices that nothing
        // new is happening and injects a course-correction, without the model having
        // to admit it is stuck.
        let responses: Vec<ChatResponse> = (0..6)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(&i.to_string(), "read", r#"{"path":"a.rs"}"#)],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::printing("same bytes every time"),
            cowboy_core::config::AgentBehavior {
                max_iterations: 6,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.stall_window = 3;
        agent.run("go in circles").await.unwrap();

        let directive = agent
            .messages
            .iter()
            .find(|m| m.role == Role::User && m.content.contains("[no progress]"))
            .expect("a directive should be injected")
            .content
            .clone();
        drop(agent);
        assert!(
            ui.notices.iter().any(|n| n.contains("no progress")),
            "the stall should be reported: {:?}",
            ui.notices
        );
        // It has to carry the measurement, not just an accusation.
        assert!(directive.contains("unchanged re-reads"));
        assert!(directive.contains("call `final`"));
    }

    #[tokio::test]
    async fn polling_with_changing_output_is_never_called_a_stall() {
        // The counting sandbox prints something different each call: a build log, a
        // health check. The novelty metric must leave this alone — the existing loop
        // guard deliberately allows it, and a second guard that didn't would break
        // every "wait for the thing to come up" loop.
        let responses: Vec<ChatResponse> = (0..6)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &i.to_string(),
                    "shell",
                    r#"{"command":"curl -s localhost:8080/health"}"#,
                )],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::counting(),
            cowboy_core::config::AgentBehavior {
                max_iterations: 6,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.stall_window = 2;
        agent.run("poll until healthy").await.unwrap();
        assert!(
            !ui.notices.iter().any(|n| n.contains("no progress")),
            "polling is not a stall: {:?}",
            ui.notices
        );
    }

    #[tokio::test]
    async fn multi_turn_retains_conversation_context() {
        // Two turns on the same loop; the conversation must accumulate.
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "final", r#"{"message":"done 1"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done 2"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let t = CancellationToken::new();
        let r1 = agent.run_turn("first task", t.clone()).await.unwrap();
        let r2 = agent.run_turn("second task", t).await.unwrap();
        assert_eq!(r1.as_deref(), Some("done 1"));
        assert_eq!(r2.as_deref(), Some("done 2"));
        // Both user turns are retained in the conversation (context preserved).
        let users = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();
        assert_eq!(users, 2);
        assert_eq!(agent.last_final.as_deref(), Some("done 2"));
    }

    #[test]
    fn concurrency_notice_reflects_the_per_provider_cap() {
        // Five subagents all on the same provider, cap 2 → 2 run, 3 queue.
        let keys = vec!["fireworks".to_string(); 5];
        let n = concurrency_notice_from_keys(&keys, 2, 4);
        assert_eq!(n, "↳ 5 subagents: 2 running, 3 queued (max 2/provider)");

        // Spread across three providers, cap 2 → all run, none queued.
        let keys = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let n = concurrency_notice_from_keys(&keys, 2, 4);
        assert_eq!(n, "↳ running 3 subagents in parallel");

        // The global max_parallel still bounds it: 3 providers each under the cap,
        // but max_parallel=2 means only 2 run at once.
        let keys = vec!["a".into(), "b".into(), "c".into()];
        let n = concurrency_notice_from_keys(&keys, 2, 2);
        assert_eq!(n, "↳ 3 subagents: 2 running, 1 queued (max 2/provider)");

        // Throttle disabled (0) → all run.
        let keys = vec!["fireworks".to_string(); 5];
        let n = concurrency_notice_from_keys(&keys, 0, 8);
        assert_eq!(n, "↳ running 5 subagents in parallel");
    }

    #[tokio::test]
    async fn subagent_respects_max_depth() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.subagent_depth = MAX_SUBAGENT_DEPTH; // already at the limit
        let err = agent
            .plan_subagent(
                &super::super::tools::SubagentArgs {
                    task: "do a thing".into(),
                    context: None,
                    category: None,
                    effort: None,
                    reason: None,
                    expected_artifact: None,
                    agent: None,
                },
                &None,
            )
            .unwrap_err();
        // At max depth it refuses to plan (no subprocess spawned).
        assert!(err.contains("depth limit"), "got: {err}");
    }

    /// Register a job without starting a process: the handle is a task that parks
    /// forever, so `stop` has something real to abort.
    fn register_fake_job(agent: &mut AgentLoop<'_>, id: &str) {
        let spec = crate::agent::jobs::JobSpec {
            id: id.into(),
            call_id: format!("call-{id}"),
            label: "tests/small".into(),
            model: "cheap".into(),
            task: "run the tests".into(),
            provider: "p".into(),
            granted: 25,
            ceiling: 400,
        };
        agent.jobs.dispatch(spec, |_, _| {
            Box::new(tokio::spawn(std::future::pending::<()>()).abort_handle())
        });
    }

    #[tokio::test]
    async fn a_finished_job_is_injected_into_the_conversation_at_the_next_boundary() {
        // The other half of async delegation: the result has to come back. It arrives as
        // a *user* message (the `subagent` call was already answered with its job id, so
        // a second tool result for that id would be a malformed conversation).
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("f", "final", r#"{"message":"all done"}"#)],
            }])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");
        agent
            .job_tx
            .send(crate::agent::jobs::JobEvent::Finished {
                id: "j1".into(),
                ok: true,
                result: "the flaky test is a missing await in store.rs".into(),
            })
            .unwrap();

        let out = agent.run("investigate the flake").await.unwrap();
        assert_eq!(out.as_deref(), Some("all done"));
        let injected = agent
            .messages
            .iter()
            .find(|m| m.role == Role::User && m.content.starts_with("[subagent"))
            .expect("the result should be injected as a user message");
        assert!(injected.content.contains("job j1"));
        assert!(injected.content.contains("missing await in store.rs"));
        // And the job is settled, so `final` was not refused.
        assert!(agent.jobs.is_idle());
    }

    #[tokio::test]
    async fn final_is_refused_while_a_subagent_is_still_running() {
        // Finishing now would throw the delegated work away: its result would land
        // after the turn that asked for it.
        let responses: Vec<ChatResponse> = (0..2)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &format!("f{i}"),
                    "final",
                    r#"{"message":"done early"}"#,
                )],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior {
                max_iterations: 2,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");

        let out = agent.run("do it").await.unwrap();
        assert!(
            out.is_none(),
            "the turn must not finish with work in flight"
        );
        let refusals: Vec<&str> = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool && m.content.starts_with("blocked:"))
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(refusals.len(), 2, "got: {refusals:?}");
        assert!(
            refusals[0].contains("subagent `j1`"),
            "got: {}",
            refusals[0]
        );
        assert!(refusals[0].contains("`wait`"), "got: {}", refusals[0]);
        agent.jobs.stop_all();
    }

    #[tokio::test]
    async fn after_repeated_refusals_the_loop_waits_rather_than_arguing() {
        // A hard refusal loop is the one way this gate could wedge a session, so past
        // the refusal bound the loop waits for the jobs itself and then lets the answer
        // through.
        let responses: Vec<ChatResponse> = (0..3)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &format!("f{i}"),
                    "final",
                    r#"{"message":"finished"}"#,
                )],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior {
                max_iterations: 3,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");
        // The worker lands shortly after the loop starts waiting.
        let tx = agent.job_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = tx.send(crate::agent::jobs::JobEvent::Finished {
                id: "j1".into(),
                ok: true,
                result: "tests pass".into(),
            });
        });

        let out = agent.run("do it").await.unwrap();
        assert_eq!(
            out.as_deref(),
            Some("finished"),
            "the third attempt should be allowed through after the wait"
        );
        assert!(agent.jobs.is_idle());
        // The result still reached the conversation rather than being dropped.
        assert!(agent
            .messages
            .iter()
            .any(|m| m.content.contains("tests pass")));
    }

    #[tokio::test]
    async fn an_interrupt_leaves_running_jobs_alone() {
        // Session-scoped jobs: cancelling the turn must not reap the children. The
        // old implementation killed the whole batch, so interrupting to say one thing
        // threw away minutes of delegated work.
        let mut ui = RecordingUi::default();
        let cancel = CancellationToken::new();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            cancel.clone(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");
        cancel.cancel();
        let out = agent.run("go").await.unwrap();
        assert!(out.is_none());
        assert_eq!(
            agent.jobs.outstanding().len(),
            1,
            "the job should still be running after an interrupt"
        );
        agent.jobs.stop_all();
    }

    /// A supervised worker: a small grant, a real ceiling, and a control directory to
    /// ask on. Returns the directory so a test can answer as the foreman would.
    fn supervised_agent<'u>(
        ui: &'u mut RecordingUi,
        responses: Vec<ChatResponse>,
        grant: u32,
        ceiling: u32,
    ) -> (
        AgentLoop<'u>,
        crate::agent::jobctl::ControlDir,
        assert_fs::TempDir,
    ) {
        let tmp = assert_fs::TempDir::new().unwrap();
        let ctl = tmp.path().join("control");
        std::fs::create_dir_all(&ctl).unwrap();
        let dir = crate::agent::jobctl::ControlDir::at(ctl);
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior {
                max_iterations: 1000,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            ui,
        );
        // Set up as if the parent had passed a grant and a channel, without touching
        // process environment (which races across tests in one binary).
        agent.budget = IterationBudget::resolve(Some(grant), Some(ceiling), 1000);
        agent.control = Some(dir.clone());
        agent.request_timeout = std::time::Duration::from_millis(400);
        (agent, dir, tmp)
    }

    /// The foreman's side, in a background task: wait for request `seq`, then answer.
    fn answer_as_foreman(dir: &crate::agent::jobctl::ControlDir, seq: u32, verdict: Verdict) {
        let dir = dir.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                if dir.read_request(seq).is_some() {
                    dir.write_verdict(&verdict).unwrap();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
    }

    #[tokio::test]
    async fn a_worker_that_spends_its_grant_reports_and_is_granted_more() {
        // The core of grant-and-request: the worker runs out, the host files a report on
        // its behalf, the foreman grants more, and the worker keeps going and finishes —
        // instead of hitting a cap and returning `[partial]`.
        let mut responses: Vec<ChatResponse> = (0..2)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &i.to_string(),
                    "read",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                )],
            })
            .collect();
        responses.push(ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call("f", "final", r#"{"message":"review complete"}"#)],
        });
        let mut ui = RecordingUi::default();
        let (mut agent, dir, _tmp) = supervised_agent(&mut ui, responses, 2, 400);
        answer_as_foreman(
            &dir,
            1,
            Verdict::Grant {
                seq: 1,
                iterations: 5,
            },
        );

        let out = agent.run("review the crate").await.unwrap();
        assert_eq!(out.as_deref(), Some("review complete"));
        // The request carried the host's evidence, not just the worker's word.
        let req = dir.read_request(1).expect("a request should be filed");
        assert_eq!(req.used, 2);
        assert_eq!(req.granted, 2);
        assert!(req.evidence.contains("files read"), "got: {}", req.evidence);
        assert!(req.evidence.contains("turns 2/2"), "got: {}", req.evidence);
        assert_eq!(agent.budget.granted, 7, "the grant should have grown by 5");
    }

    #[tokio::test]
    async fn an_unanswered_request_extends_once_then_wraps_up() {
        // The policy that matters most: an unattended foreman must neither let a worker
        // run forever nor destroy its work. One small extension, then wrap up — and the
        // worker still ends with a real answer.
        let responses: Vec<ChatResponse> = (0..40)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &i.to_string(),
                    "read",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                )],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let (mut agent, _dir, _tmp) = supervised_agent(&mut ui, responses, 2, 400);
        // Nobody answers.
        let out = agent.run("wander forever").await.unwrap();
        assert!(out.is_none(), "it ran out of turns rather than finishing");
        // Derived, not hardcoded: grant + one automatic extension + the wrap-up
        // allowance. Spelling the total as a literal meant bumping WRAP_UP_TURNS broke
        // this test for a reason unrelated to what it checks, which is boundedness.
        let expected = 2 + AUTO_EXTENSION_TURNS + crate::agent::jobctl::WRAP_UP_TURNS;
        assert_eq!(agent.budget.granted, expected, "bounded, not unbounded");
        assert_eq!(agent.turn_requests, 2, "it asked twice and stopped asking");
        drop(agent);
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("no answer from the foreman")),
            "{:?}",
            ui.notices
        );
    }

    #[tokio::test]
    async fn a_wrap_up_verdict_leaves_enough_turns_to_write_the_answer() {
        // "Report now" with no turns to report in would produce exactly the empty
        // result this mechanism exists to prevent.
        let responses = vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("0", "read", r#"{"path":"a.rs"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "f",
                    "final",
                    r#"{"message":"partial but reported: found one issue"}"#,
                )],
            },
        ];
        let mut ui = RecordingUi::default();
        let (mut agent, dir, _tmp) = supervised_agent(&mut ui, responses, 1, 400);
        answer_as_foreman(&dir, 1, Verdict::WrapUp { seq: 1 });

        let out = agent.run("review it").await.unwrap();
        assert_eq!(
            out.as_deref(),
            Some("partial but reported: found one issue")
        );
        assert!(
            agent
                .messages
                .iter()
                .any(|m| m.content.contains("[wrap up]")),
            "the worker should be told to write up"
        );
    }

    #[tokio::test]
    async fn a_redirect_changes_direction_and_clears_the_stall_streak() {
        // Two reads spend the grant; after the redirect the worker does what it was
        // told and finishes, so the redirect is the only request.
        let mut responses: Vec<ChatResponse> = (0..2)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(&i.to_string(), "read", r#"{"path":"a.rs"}"#)],
            })
            .collect();
        responses.push(ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call(
                "f",
                "final",
                r#"{"message":"checked the error paths"}"#,
            )],
        });
        let mut ui = RecordingUi::default();
        let (mut agent, dir, _tmp) = supervised_agent(&mut ui, responses, 2, 400);
        answer_as_foreman(
            &dir,
            1,
            Verdict::Redirect {
                seq: 1,
                iterations: 3,
                instructions: "look at the error paths in store.rs instead".into(),
            },
        );
        let out = agent.run("review it").await.unwrap();
        assert_eq!(out.as_deref(), Some("checked the error paths"));
        let redirect = agent
            .messages
            .iter()
            .find(|m| m.content.contains("[foreman] Change of direction"))
            .expect("the instructions should reach the worker");
        assert!(redirect.content.contains("error paths in store.rs"));
        // It got turns to act on the redirect, and the stall streak it may have been
        // reported for was cleared at that point (see `a_redirect_can_clear_the_streak`
        // for the streak logic itself).
        assert_eq!(agent.budget.granted, 5);
        assert_eq!(agent.turn_requests, 1);
        assert_eq!(agent.progress.barren_streak(), 0);
    }

    #[tokio::test]
    async fn a_stop_verdict_ends_the_work_with_a_report_not_silence() {
        let responses = vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("0", "read", r#"{"path":"a.rs"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    "f",
                    "final",
                    r#"{"message":"stopped; had established the schema is fine"}"#,
                )],
            },
        ];
        let mut ui = RecordingUi::default();
        let (mut agent, dir, _tmp) = supervised_agent(&mut ui, responses, 1, 400);
        answer_as_foreman(
            &dir,
            1,
            Verdict::Stop {
                seq: 1,
                reason: "the approach was wrong".into(),
            },
        );
        let out = agent.run("review it").await.unwrap();
        assert!(out.unwrap().contains("stopped"));
        assert!(agent
            .messages
            .iter()
            .any(|m| m.content.contains("the approach was wrong")));
    }

    #[tokio::test]
    async fn the_voluntary_request_tool_asks_and_reports_the_answer() {
        let mut ui = RecordingUi::default();
        let (mut agent, dir, _tmp) = supervised_agent(&mut ui, vec![], 40, 400);
        answer_as_foreman(
            &dir,
            1,
            Verdict::Grant {
                seq: 1,
                iterations: 25,
            },
        );
        let out = agent
            .run_request_turns(&tools::RequestTurnsArgs {
                progress: "mapped the crate and reviewed 4 of 9 files".into(),
                remaining: "5 files".into(),
                next_step: "review journal.rs".into(),
                iterations: 25,
            })
            .await;
        assert!(out.contains("granted"), "got: {out}");
        assert_eq!(agent.budget.granted, 65);
        let req = dir.read_request(1).unwrap();
        assert!(req.report.contains("reviewed 4 of 9 files"));
        assert!(req.report.contains("Next step: review journal.rs"));
    }

    #[tokio::test]
    async fn a_worker_with_no_foreman_is_told_so_rather_than_left_hanging() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let out = agent
            .run_request_turns(&tools::RequestTurnsArgs {
                progress: "p".into(),
                remaining: "r".into(),
                next_step: "n".into(),
                iterations: 10,
            })
            .await;
        assert!(out.contains("no foreman to ask"), "got: {out}");
    }

    #[tokio::test]
    async fn asking_is_bounded_so_a_worker_and_foreman_cannot_negotiate_forever() {
        let mut ui = RecordingUi::default();
        let (mut agent, _dir, _tmp) = supervised_agent(&mut ui, vec![], 10, 100_000);
        agent.turn_requests = MAX_TURN_REQUESTS;
        let out = agent
            .run_request_turns(&tools::RequestTurnsArgs {
                progress: "p".into(),
                remaining: "r".into(),
                next_step: "n".into(),
                iterations: 10,
            })
            .await;
        assert!(out.contains("write up what you have"), "got: {out}");
    }

    #[tokio::test]
    async fn a_grant_that_hits_the_ceiling_becomes_a_wrap_up() {
        // The foreman can say "more turns"; the host still gets the last word.
        let mut ui = RecordingUi::default();
        let (mut agent, dir, _tmp) = supervised_agent(&mut ui, vec![], 40, 40);
        answer_as_foreman(
            &dir,
            1,
            Verdict::Grant {
                seq: 1,
                iterations: 500,
            },
        );
        let out = agent
            .run_request_turns(&tools::RequestTurnsArgs {
                progress: "p".into(),
                remaining: "r".into(),
                next_step: "n".into(),
                iterations: 500,
            })
            .await;
        assert!(out.contains("call\n                 `final`") || out.contains("`final`"));
        assert_eq!(agent.budget.granted, 40, "the ceiling holds");
        assert!(agent
            .messages
            .iter()
            .any(|m| m.content.contains("[wrap up]")));
    }

    #[tokio::test]
    async fn a_worker_whose_parent_was_killed_stops_itself() {
        // Session-scoped jobs are only reaped by a parent that asks. A parent killed
        // outright never asks, so the worker has to notice — otherwise a `kill -9` on the
        // worker process leaves children spending on results nobody will read.
        let responses: Vec<ChatResponse> = (0..4)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &i.to_string(),
                    "shell",
                    r#"{"command":"echo hi"}"#,
                )],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        // A pid that cannot exist: the parent is gone.
        agent.parent_pid = Some(u32::MAX);
        let out = agent.run("keep working").await.unwrap();
        assert!(out.is_none());
        drop(agent);
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("parent session is gone")),
            "{:?}",
            ui.notices
        );
        // It stopped before doing any work, rather than after burning its grant.
        assert!(ui.commands.is_empty(), "{:?}", ui.commands);
    }

    #[tokio::test]
    async fn a_live_parent_does_not_stop_the_worker() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("f", "final", r#"{"message":"done"}"#)],
            }])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.parent_pid = Some(std::process::id());
        assert_eq!(
            agent.run("work").await.unwrap().as_deref(),
            Some("done"),
            "a live parent must not be mistaken for a dead one"
        );
    }

    #[tokio::test]
    async fn a_worker_waiting_on_a_dead_parent_stops_instead_of_waiting_out_the_timeout() {
        let mut ui = RecordingUi::default();
        let (mut agent, _dir, _tmp) = supervised_agent(&mut ui, vec![], 40, 400);
        agent.parent_pid = Some(u32::MAX);
        agent.request_timeout = std::time::Duration::from_secs(600);
        let started = std::time::Instant::now();
        let out = agent
            .run_request_turns(&tools::RequestTurnsArgs {
                progress: "p".into(),
                remaining: "r".into(),
                next_step: "n".into(),
                iterations: 10,
            })
            .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "it must not wait out the request timeout for an answer that cannot come"
        );
        assert!(out.contains("interrupted"), "got: {out}");
    }

    #[tokio::test]
    async fn user_input_reaches_a_running_turn_at_the_next_step() {
        // Typing while the agent works used to mean waiting for the whole turn. Now the
        // message lands on the next iteration — without cancelling anything.
        let responses: Vec<ChatResponse> = vec![
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("0", "shell", r#"{"command":"echo one"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"echo two"}"#)],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("f", "final", r#"{"message":"did both"}"#)],
            },
        ];
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        // As if the user typed while the turn was already running.
        agent
            .steer_sender()
            .send("also check the error path".into())
            .unwrap();

        let out = agent.run("do the thing").await.unwrap();
        assert_eq!(out.as_deref(), Some("did both"), "the turn still completed");
        let steered = agent
            .messages
            .iter()
            .find(|m| m.role == Role::User && m.content.contains("while you are working"))
            .expect("the message should be injected into the running turn");
        assert!(steered.content.contains("also check the error path"));
        assert!(
            steered.content.contains("Do not restart"),
            "steering is a correction, not a new task: {}",
            steered.content
        );
        // Both commands still ran: steering does not cancel work in progress.
        drop(agent);
        assert_eq!(ui.commands.len(), 2, "{:?}", ui.commands);
    }

    #[tokio::test]
    async fn steering_breaks_a_wait_rather_than_being_stuck_behind_it() {
        // `wait` is the one place the loop blocks on purpose. If user input could not
        // interrupt it, parking on a subagent would recreate exactly the unresponsive
        // session this change is about.
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");
        let steer = agent.steer_sender();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let _ = steer.send("stop looking at that, check the migration".into());
        });
        let started = std::time::Instant::now();
        let out = agent
            .run_wait(&tools::WaitArgs {
                timeout_seconds: Some(600),
                ..Default::default()
            })
            .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the wait must end when the user speaks"
        );
        assert!(out.contains("the user said something"), "got: {out}");
        assert!(agent
            .messages
            .iter()
            .any(|m| m.content.contains("check the migration")));
        agent.jobs.stop_all();
    }

    #[tokio::test]
    async fn empty_steering_is_ignored() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.steer_sender().send("   ".into()).unwrap();
        assert_eq!(agent.drain_steering(), 0);
        assert!(agent.messages.iter().all(|m| m.role != Role::User));
    }

    #[tokio::test]
    async fn a_second_stall_escalates_to_the_foreman_instead_of_repeating_itself() {
        // Task 3's directive is worth saying once. A worker that ignores it is not going
        // to be talked out of the loop, so the host escalates: the foreman gets the
        // report plus the measured evidence and decides.
        //
        // Driven with a `plan` call — bookkeeping, which produces no new files, edits or
        // commands — and a one-iteration stall window, with one stall already recorded.
        let responses: Vec<ChatResponse> = (0..4)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(
                    &i.to_string(),
                    "plan",
                    r#"{"steps":[{"step":"think about it","status":"in_progress"}]}"#,
                )],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let (mut agent, dir, _tmp) = supervised_agent(&mut ui, responses, 4, 400);
        agent.stall_window = 1;
        agent.stall_count = 1; // the directive has already been given once
        answer_as_foreman(&dir, 1, Verdict::WrapUp { seq: 1 });

        agent.run("go in circles").await.unwrap();
        let req = dir
            .read_request(1)
            .expect("the second stall should reach the foreman");
        assert!(req.report.contains("appears stuck"), "got: {}", req.report);
        // The evidence is the host's measurement, not the worker's account of itself.
        assert!(
            req.evidence.contains("nothing new"),
            "got: {}",
            req.evidence
        );
        assert!(req.evidence.contains("turns"), "got: {}", req.evidence);
        assert!(agent.wrapping_up, "the verdict should be terminal");
    }

    #[tokio::test]
    async fn jobs_lists_state_and_the_budget_columns() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        assert!(agent.run_jobs().contains("no background jobs"));

        register_fake_job(&mut agent, "j1");
        let out = agent.run_jobs();
        assert!(out.contains("`j1`"), "got: {out}");
        assert!(out.contains("pending"), "got: {out}");
        // The foreman has to be able to see what an extension would cost.
        assert!(out.contains("turns 0/25"), "got: {out}");
        assert!(out.contains("ceiling 400"), "got: {out}");

        // Once it asks for turns, the ask is visible and called out.
        agent
            .jobs
            .apply_event(crate::agent::jobs::JobEvent::TurnRequest {
                id: "j1".into(),
                seq: 1,
                report: "half done".into(),
                requested: 30,
                used: 25,
            });
        let out = agent.run_jobs();
        assert!(out.contains("asking for 30 more"), "got: {out}");
        assert!(out.contains("`job_reply`"), "got: {out}");
        agent.jobs.stop_all();
    }

    #[tokio::test]
    async fn wait_returns_when_a_job_lands_and_when_it_times_out() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        // Nothing running: `wait` is a no-op rather than a hang.
        assert!(agent
            .run_wait(&tools::WaitArgs::default())
            .await
            .contains("nothing to wait for"));

        register_fake_job(&mut agent, "j1");
        // Times out rather than parking forever, and says what to do next.
        let out = agent
            .run_wait(&tools::WaitArgs {
                timeout_seconds: Some(1),
                ..Default::default()
            })
            .await;
        assert!(out.contains("timed out"), "got: {out}");
        assert!(out.contains("1 job(s) still running"), "got: {out}");

        // A worker landing wakes it immediately.
        let tx = agent.job_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let _ = tx.send(crate::agent::jobs::JobEvent::Finished {
                id: "j1".into(),
                ok: true,
                result: "all green".into(),
            });
        });
        let out = agent
            .run_wait(&tools::WaitArgs {
                timeout_seconds: Some(30),
                ..Default::default()
            })
            .await;
        assert!(out.contains("job update(s) arrived"), "got: {out}");
        assert!(agent.jobs.is_idle());
        // The result reached the conversation, not just the wait's return value.
        assert!(agent
            .messages
            .iter()
            .any(|m| m.content.contains("all green")));
    }

    #[tokio::test]
    async fn wait_is_interruptible() {
        // An interrupt must never be swallowed by a wait: this is the failure the whole
        // change is about, and `wait` is the one place the loop deliberately blocks.
        let cancel = CancellationToken::new();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            cancel.clone(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            c.cancel();
        });
        let started = std::time::Instant::now();
        let out = agent
            .run_wait(&tools::WaitArgs {
                timeout_seconds: Some(600),
                ..Default::default()
            })
            .await;
        assert!(out.contains("interrupted"), "got: {out}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "an interrupt must not wait out the timeout"
        );
        agent.jobs.stop_all();
    }

    #[tokio::test]
    async fn wait_reports_an_unknown_job_instead_of_waiting_for_everything() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");
        let out = agent
            .run_wait(&tools::WaitArgs {
                ids: Some(vec!["nope".into()]),
                timeout_seconds: Some(1),
                ..Default::default()
            })
            .await;
        assert!(out.contains("no such job"), "got: {out}");
        agent.jobs.stop_all();
    }

    #[tokio::test]
    async fn a_subagents_question_is_delivered_to_the_foreman_and_answered() {
        // The whole point of the feature: a worker that hits an ambiguity gets a real
        // answer from the session that has the context, instead of the empty string that
        // used to mean "guess".
        let root = assert_fs::TempDir::new().unwrap();
        let mut ui = RecordingUi::default();
        let logger =
            crate::session::SessionLogger::create_with_id(root.path(), "job-reply-answer").unwrap();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::at(root.path().to_path_buf()),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        register_fake_job(&mut agent, "j1");

        agent
            .jobs
            .apply_event(crate::agent::jobs::JobEvent::Question {
                id: "j1".into(),
                seq: 1,
                question: "migrate the v1 endpoints too?".into(),
                options: vec!["yes".into(), "no".into()],
            });

        // It reaches the conversation as a message the foreman can act on, with the
        // suggested answers and the instruction for how to reply.
        agent.deliver_job_news();
        let note = agent
            .messages
            .iter()
            .rev()
            .map(|m| m.content.clone())
            .find(|c| c.contains("v1 endpoints"))
            .unwrap_or_default();
        assert!(
            note.contains("v1 endpoints"),
            "the question should reach the conversation"
        );
        assert!(note.contains("yes · no"), "the options should show: {note}");
        assert!(note.contains("job_reply"), "got: {note}");

        // An `answer` with nothing in `instructions` is refused rather than sending an
        // empty reply, which the worker would read as a real answer of "".
        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "answer".into(),
            iterations: None,
            instructions: None,
        });
        assert!(
            out.contains("put your reply in `instructions`"),
            "got: {out}"
        );
        assert_eq!(
            agent.jobs.get("j1").unwrap().state,
            crate::agent::jobs::JobState::AwaitingAnswer { seq: 1 }
        );

        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "answer".into(),
            iterations: None,
            instructions: Some("no — v1 is being retired next quarter".into()),
        });
        assert!(out.contains("answered job"), "got: {out}");
        assert_eq!(
            agent.jobs.get("j1").unwrap().state,
            crate::agent::jobs::JobState::Running,
            "answering resumes the job"
        );

        // And the answer really landed in the control channel the worker polls.
        let dir = crate::agent::jobctl::ControlDir::create("job-reply-answer", "j1").unwrap();
        let answer = dir.read_answer(1).expect("the answer should be on disk");
        assert!(answer.answer.contains("retired next quarter"));

        agent.stop_all_jobs();
    }

    #[tokio::test]
    async fn answering_a_job_that_is_asking_for_turns_says_so() {
        // `answer` is a real verdict, so using it on a budget request should explain the
        // mismatch rather than report "unknown verdict".
        let root = assert_fs::TempDir::new().unwrap();
        let mut ui = RecordingUi::default();
        let logger =
            crate::session::SessionLogger::create_with_id(root.path(), "job-reply-mismatch")
                .unwrap();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::at(root.path().to_path_buf()),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        register_fake_job(&mut agent, "j1");
        agent
            .jobs
            .apply_event(crate::agent::jobs::JobEvent::TurnRequest {
                id: "j1".into(),
                seq: 1,
                report: "half done".into(),
                requested: 30,
                used: 25,
            });
        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "answer".into(),
            iterations: None,
            instructions: Some("sure, go ahead".into()),
        });
        assert!(
            out.contains("asking for turns, not asking a question"),
            "got: {out}"
        );
        agent.stop_all_jobs();
    }

    #[tokio::test]
    async fn job_reply_clamps_a_grant_to_the_host_ceiling() {
        // The foreman decides *whether* to extend; the host decides how far the bound
        // goes. A foreman granting 10_000 turns must not be able to.
        let root = assert_fs::TempDir::new().unwrap();
        let mut ui = RecordingUi::default();
        let logger =
            crate::session::SessionLogger::create_with_id(root.path(), "job-reply-clamp").unwrap();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::at(root.path().to_path_buf()),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        register_fake_job(&mut agent, "j1");
        // Not waiting on anything yet: there is nothing to answer.
        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "grant".into(),
            iterations: Some(10),
            instructions: None,
        });
        assert!(out.contains("not waiting for a verdict"), "got: {out}");

        agent
            .jobs
            .apply_event(crate::agent::jobs::JobEvent::TurnRequest {
                id: "j1".into(),
                seq: 1,
                report: "half done".into(),
                requested: 30,
                used: 25,
            });
        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "grant".into(),
            iterations: Some(10_000),
            instructions: None,
        });
        assert!(out.contains("clamped"), "got: {out}");
        let job = agent.jobs.get("j1").unwrap();
        assert_eq!(job.granted, 400, "the ceiling is the bound, not the ask");
        assert_eq!(job.state, crate::agent::jobs::JobState::Running);
        // `stop_all_jobs`, not `jobs.stop_all`: it also removes the job's host-side
        // control directory, so a test run leaves nothing under $XDG_STATE_HOME.
        agent.stop_all_jobs();
    }

    #[tokio::test]
    async fn job_reply_validates_the_verdict_and_requires_redirect_instructions() {
        let root = assert_fs::TempDir::new().unwrap();
        let mut ui = RecordingUi::default();
        let logger =
            crate::session::SessionLogger::create_with_id(root.path(), "job-reply-validate")
                .unwrap();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::at(root.path().to_path_buf()),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        register_fake_job(&mut agent, "j1");
        agent
            .jobs
            .apply_event(crate::agent::jobs::JobEvent::TurnRequest {
                id: "j1".into(),
                seq: 1,
                report: "stuck".into(),
                requested: 30,
                used: 25,
            });

        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "sure why not".into(),
            iterations: None,
            instructions: None,
        });
        assert!(out.contains("unknown verdict"), "got: {out}");

        // A redirect with nothing to redirect *to* is refused rather than sent.
        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "redirect".into(),
            iterations: Some(10),
            instructions: None,
        });
        assert!(out.contains("needs `instructions`"), "got: {out}");
        // Still waiting, because neither reply was delivered.
        assert!(matches!(
            agent.jobs.get("j1").unwrap().state,
            crate::agent::jobs::JobState::AwaitingVerdict { .. }
        ));

        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "unknown-job".into(),
            iterations: None,
            instructions: None,
        });
        assert!(out.contains("unknown verdict"), "got: {out}");
        agent.stop_all_jobs();
    }

    #[tokio::test]
    async fn a_result_from_before_an_interrupt_arrives_in_the_next_turn() {
        // The payoff of session-scoped jobs: the user interrupts to correct the
        // foreman, the delegated work keeps going, and its result lands in the turn
        // after. Under the old batch-join this work was killed and re-done.
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("f", "final", r#"{"message":"folded it in"}"#)],
            }])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");

        // Turn one is interrupted before it does anything.
        let interrupted = CancellationToken::new();
        interrupted.cancel();
        assert!(agent
            .run_turn("start the review", interrupted)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            agent.running_jobs(),
            1,
            "the worker must survive the cancel"
        );

        // The worker lands while the session sits idle.
        agent
            .job_tx
            .send(crate::agent::jobs::JobEvent::Finished {
                id: "j1".into(),
                ok: true,
                result: "review done: two real findings".into(),
            })
            .unwrap();

        // The next turn picks it up.
        let out = agent
            .run_turn(
                "actually, focus on the error paths",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("folded it in"));
        assert!(
            agent
                .messages
                .iter()
                .any(|m| m.content.contains("two real findings")),
            "the pre-interrupt result should reach the next turn"
        );
    }

    #[tokio::test]
    async fn stopping_the_subagents_settles_them_without_touching_the_turn() {
        // The interrupt this change adds: kill the delegated work, keep the session.
        // Fired through the shared switch, which is what lets the worker honour it
        // while a turn holds `&mut` on the loop.
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        register_fake_job(&mut agent, "j1");
        register_fake_job(&mut agent, "j2");
        assert_eq!(agent.running_jobs(), 2);

        let stopped = agent.stop_all_jobs();
        assert_eq!(stopped, 2);
        assert_eq!(agent.running_jobs(), 0);
        // The turn's own cancellation is untouched — stopping subagents is not
        // stopping the foreman.
        assert!(!agent.cancel.is_cancelled());

        // And the foreman is told, so it does not sit waiting for results that will
        // never come.
        agent.deliver_job_news();
        let notes: Vec<&str> = agent
            .messages
            .iter()
            .filter(|m| m.content.contains("[stopped]"))
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(notes.len(), 2, "both jobs should report themselves stopped");
        assert!(notes[0].contains("resume from that checkpoint"));
    }

    #[tokio::test]
    async fn the_stop_switch_reaches_a_job_that_is_actually_running() {
        // `stop_all_jobs` aborts the task; here the task is the one that matters — a
        // long-lived future standing in for a child process — and it must be gone
        // rather than left detached.
        let mut ui = RecordingUi::default();
        let agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let stop = agent.job_stopper();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        let token = stop.token();
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
        // Fired from a clone held outside the loop, exactly as the worker does.
        stop.stop_all();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("the job task must observe the stop")
            .unwrap();
        assert!(
            !ran.load(std::sync::atomic::Ordering::Relaxed),
            "the job should have been stopped, not run to completion"
        );
        // Re-armed, so the next dispatch is not born cancelled.
        assert!(!agent.job_stopper().token().is_cancelled());
    }

    #[tokio::test]
    async fn stopping_a_job_from_job_reply_settles_it_and_tells_the_foreman() {
        let root = assert_fs::TempDir::new().unwrap();
        let mut ui = RecordingUi::default();
        let logger =
            crate::session::SessionLogger::create_with_id(root.path(), "job-reply-stop").unwrap();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::at(root.path().to_path_buf()),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        register_fake_job(&mut agent, "j1");
        agent
            .jobs
            .apply_event(crate::agent::jobs::JobEvent::TurnRequest {
                id: "j1".into(),
                seq: 1,
                report: "going in circles".into(),
                requested: 50,
                used: 25,
            });
        let out = agent.run_job_reply(&tools::JobReplyArgs {
            id: "j1".into(),
            verdict: "stop".into(),
            iterations: None,
            instructions: Some("the approach is wrong".into()),
        });
        assert!(out.contains("stopped job"), "got: {out}");
        // A stopped job must not be left outstanding — it is blocked waiting on us, so
        // nothing else would ever settle it.
        assert!(agent.jobs.is_idle());
        assert!(agent
            .messages
            .iter()
            .any(|m| m.content.contains("[stopped]")));
    }

    #[tokio::test]
    async fn coordination_calls_do_not_trip_the_loop_guard() {
        // A foreman with work in flight calls `jobs` repeatedly, with identical
        // arguments and identical output. That is exactly the shape the repetition
        // guard aborts a turn for, so it has to be exempt — otherwise supervising a
        // crew would end the turn.
        let responses: Vec<ChatResponse> = (0..8)
            .map(|i| ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call(&i.to_string(), "jobs", "{}")],
            })
            .collect();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(responses)),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior {
                max_iterations: 8,
                ..Default::default()
            },
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.stall_window = 3;
        register_fake_job(&mut agent, "j1");
        agent.run("supervise").await.unwrap();
        agent.jobs.stop_all();
        drop(agent);

        assert!(
            !ui.notices.iter().any(|n| n.contains("loop")),
            "coordination must not read as a loop: {:?}",
            ui.notices
        );
        assert!(
            !ui.notices.iter().any(|n| n.contains("no progress")),
            "waiting on a worker is not going in circles: {:?}",
            ui.notices
        );
    }

    #[tokio::test]
    async fn dispatch_answers_every_call_by_id() {
        // Three delegations in one turn. At max depth they all short-circuit in
        // planning (no subprocess), but we still get one result per call id —
        // proving the batch maps every subagent call.
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.subagent_depth = MAX_SUBAGENT_DEPTH;
        let calls = vec![
            tool_call(
                "a",
                "subagent",
                r#"{"task":"x","category":"tests","effort":"small"}"#,
            ),
            tool_call(
                "b",
                "subagent",
                r#"{"task":"y","category":"review","effort":"deep"}"#,
            ),
            tool_call("c", "shell", r#"{"command":"echo hi"}"#), // non-subagent ignored
        ];
        let results = agent.dispatch_subagents(&calls);
        assert_eq!(results.len(), 2, "only subagent calls produce results");
        assert!(results.contains_key("a") && results.contains_key("b"));
        assert!(!results.contains_key("c"));
        assert!(results["a"].contains("depth limit"));
        // A refused delegation is not a job.
        assert!(agent.jobs.is_idle());
    }

    #[tokio::test]
    async fn dispatch_does_not_block_on_the_children_it_starts() {
        // The defect this whole change exists to fix: dispatch must return before the
        // workers do. Two delegations are started with a cancelled token — which the
        // old join-the-batch implementation treated as "salvage and give up" — and the
        // dispatch still hands back a job id per call without awaiting anything.
        let mut ui = RecordingUi::default();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            cancel,
            &mut ui,
        );
        let calls = vec![
            tool_call(
                "a",
                "subagent",
                r#"{"task":"x","category":"tests","effort":"small"}"#,
            ),
            tool_call(
                "b",
                "subagent",
                r#"{"task":"y","category":"docs","effort":"tiny"}"#,
            ),
        ];
        let results = agent.dispatch_subagents(&calls);
        assert_eq!(results.len(), 2);
        for id in ["a", "b"] {
            assert!(
                results[id].starts_with("dispatched:"),
                "the call is answered with a job, not an answer: {}",
                results[id]
            );
        }
        // Both jobs are registered and outstanding, so `final` is now premature.
        assert_eq!(agent.jobs.outstanding().len(), 2);
        assert!(!agent.jobs.is_idle());
        let summary = agent.outstanding_jobs_summary();
        assert!(summary.contains("2 subagents"), "got: {summary}");
        // Nothing may leak: the children are host processes, so stop them explicitly.
        agent.jobs.stop_all();
    }

    /// The tool schemas go out with every request, so the budget has to know about
    /// them. It used to be `max(max_output, RESPONSE_HEADROOM)`, where `max` meant the
    /// floor that supposedly covered schemas was discarded as soon as a model's output
    /// budget exceeded it — i.e. always, in practice.
    /// Every tool result is capped, whatever produced it. Asserted through the real
    /// dispatch rather than by calling the helper, because the bug was that three arms
    /// never reached the helper: `subagent` (a child process's whole stdout, times
    /// however many ran in parallel), `mcp` (third-party bytes), and `memory recall`.
    /// After `--resume`, the task is **not** at index 1 — `with_history` puts the
    /// previous session's transcript there. `pinned()` used to return 2 for any user
    /// message in that slot, so a resumed session pinned the *old* session's first
    /// message and left the real task exposed to the next fold.
    #[tokio::test]
    async fn a_resumed_session_pins_the_current_task_not_the_old_one() {
        let mut ui = RecordingUi::default();
        let history = vec![
            Message::user("LAST SESSION: add a parser"),
            Message::new(Role::Assistant, "did the parser"),
        ];
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_history(history);
        // What `run_inner` does when the turn starts.
        agent
            .messages
            .push(Message::user("THE TASK: migrate the db"));
        agent.task = Some("THE TASK: migrate the db".into());

        // The resumed message is not the task, so only the system prompt is head-pinned.
        assert_eq!(agent.pinned(), 1);

        for i in 0..60 {
            agent.messages.push(Message::new(
                Role::Assistant,
                format!("intermediate step {i} with plenty of words to spend budget"),
            ));
        }
        agent.drop_oldest(60);

        assert_eq!(agent.messages[0].role, Role::System, "system kept");
        let contents: Vec<&String> = agent.messages.iter().map(|m| &m.content).collect();
        assert!(
            contents
                .iter()
                .any(|c| c.contains("THE TASK: migrate the db")),
            "the current task must survive: {contents:?}"
        );
        assert!(
            !contents.iter().any(|c| c.contains("LAST SESSION")),
            "stale resumed history should be droppable, not pinned: {contents:?}"
        );
    }

    /// The same protection has to hold when history is *summarized* rather than
    /// dropped: a fold whose span contains the task must carry it through verbatim.
    #[tokio::test]
    async fn compaction_carries_the_task_through_verbatim() {
        let mut ui = RecordingUi::default();
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: Some("SUMMARY: earlier work".into()),
            tool_calls: vec![],
        }]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_history(vec![Message::user("LAST SESSION: something else")]);
        agent.messages.push(Message::user("THE TASK: ship the fix"));
        agent.task = Some("THE TASK: ship the fix".into());
        for i in 0..12 {
            agent
                .messages
                .push(Message::new(Role::Assistant, format!("step {i} detail")));
            agent.messages.push(Message::user(format!("next {i}")));
        }
        set_context_budget(&mut agent, 60);
        agent.fit_context().await;

        let contents: Vec<&String> = agent.messages.iter().map(|m| &m.content).collect();
        assert!(
            contents
                .iter()
                .any(|c| c.contains("THE TASK: ship the fix")),
            "the task must survive compaction verbatim: {contents:?}"
        );
    }

    /// A resume must not drag in an unbounded transcript. The file on disk has no
    /// relationship to the window of whatever model is resuming it, so loading it whole
    /// either overflowed the first request or paid for a compaction call that
    /// immediately discarded most of what was just read.
    #[tokio::test]
    async fn resumed_history_is_bounded_to_part_of_the_budget() {
        let mut ui = RecordingUi::default();
        // A long prior session: 400 turns of real text.
        let history: Vec<Message> = (0..400)
            .flat_map(|i| {
                [
                    Message::user(format!("prior request {i} with a fair few words in it")),
                    Message::new(
                        Role::Assistant,
                        format!("prior answer {i} with a fair few words in it too"),
                    ),
                ]
            })
            .collect();
        // A modest window, so the allowance is smaller than the transcript — which is
        // the situation being guarded against.
        let agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            20_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_history(history.clone());

        let loaded = agent.messages.len() - 1; // minus the system prompt
        assert!(
            loaded < history.len(),
            "the whole transcript should not be loaded ({loaded} of {})",
            history.len()
        );
        // It kept the *newest* end, which is the part that matters for continuing.
        let contents: Vec<&String> = agent.messages.iter().map(|m| &m.content).collect();
        assert!(
            contents.iter().any(|c| c.contains("prior answer 399")),
            "the most recent history should be kept"
        );
        assert!(
            !contents.iter().any(|c| c.contains("prior request 0")),
            "the oldest history should be dropped"
        );
        // And what was loaded fits the allowance it was given.
        let used: usize = agent.messages[1..]
            .iter()
            .map(AgentLoop::message_tokens)
            .sum();
        assert!(
            used <= agent.context_budget() / 2 + 64,
            "loaded {used} tokens against an allowance of {}",
            agent.context_budget() / 2
        );
    }

    /// A bounded resume must not start mid-tool-call: a `Tool` result whose call was
    /// trimmed away, or an assistant turn whose results were, is the shape providers
    /// reject outright.
    #[test]
    fn a_bounded_resume_does_not_begin_mid_tool_call() {
        let history = vec![
            Message::user("older"),
            {
                let mut m = Message::new(Role::Assistant, "calling a tool");
                m.tool_calls = vec![ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                }];
                m
            },
            Message::tool_result("c1", "the result"),
            Message::new(Role::Assistant, "a clean finish"),
        ];
        // An allowance small enough to cut into the tool exchange.
        let kept = AgentLoop::tail_within(history, 12);
        assert!(
            kept.first().is_none_or(|m| m.role != Role::Tool),
            "must not lead with an orphaned tool result: {kept:?}"
        );
        assert!(
            kept.first().is_none_or(|m| m.tool_calls.is_empty()),
            "must not lead with an unanswered tool call: {kept:?}"
        );
    }

    /// The bound must not be so eager that a short resume loses anything.
    #[test]
    fn a_short_resume_is_kept_whole() {
        let history = vec![
            Message::user("just one exchange"),
            Message::new(Role::Assistant, "and its answer"),
        ];
        let kept = AgentLoop::tail_within(history.clone(), 100_000);
        assert_eq!(kept.len(), history.len());
    }

    /// tiktoken is slow enough that repeated full passes dominate the loop's own cost,
    /// so message token counts are memoized. This asserts the memo actually *bites* —
    /// by inspecting the memo directly rather than timing two passes, which flaked
    /// under concurrent test load (a wall-clock `warm < cold/5` ratio is not stable
    /// when the CPU is contended).
    #[tokio::test]
    async fn repeated_token_counts_are_memoized() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let body = "the quick brown fox jumps over the lazy dog ".repeat(40);
        for _ in 0..120 {
            agent.messages.push(Message::user(body.clone()));
        }
        assert!(agent.token_memo.borrow().is_empty(), "memo starts empty");

        let cold_total = agent.total_tokens();
        // The conversation is the seeded system message plus 120 identical user
        // messages. The 120 identical ones share one memo key, so the memo holds
        // exactly two entries (system + the shared user body): proof of both
        // memoization and cross-message dedup. A non-memoizing implementation would
        // still return the right total, so the memo state is the only faithful witness
        // that it bit.
        assert_eq!(
            agent.token_memo.borrow().len(),
            2,
            "system + 120 identical user messages must collapse to two memo entries"
        );

        let warm_total = agent.total_tokens();
        assert_eq!(
            cold_total, warm_total,
            "the memo must not change the answer"
        );
        assert!(cold_total > 1000, "the fixture should be substantial");
        assert_eq!(
            agent.token_memo.borrow().len(),
            2,
            "a second pass adds no new entries — every message was a memo hit"
        );

        // Changing a message changes its key, so the count follows the content rather
        // than going stale — which is the whole reason for hashing instead of tracking
        // mutations.
        let mut m = Message::new(Role::Assistant, "short");
        let before = agent.tokens_of(&m);
        m.reasoning = Some(body.clone());
        let after = agent.tokens_of(&m);
        assert!(
            after > before,
            "adding reasoning must raise the count ({before} -> {after})"
        );
    }

    /// A fold has to shrink things. Nothing enforced that the summary was smaller than
    /// what it replaced: `fit_context` leaves 40% of the budget for the system prompt,
    /// the task and the summary, but the model is free to return an essay — and a
    /// summary that overflows its own allowance turns one compaction into a loop of
    /// them.
    #[tokio::test]
    async fn a_runaway_compaction_summary_is_capped() {
        let mut ui = RecordingUi::default();
        // The "summary" is longer than the conversation it is meant to condense.
        let essay = "and then a great many further details followed. ".repeat(4_000);
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: Some(essay.clone()),
            tool_calls: vec![],
        }]);
        let agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let cap = agent.summary_token_cap();
        let got = agent
            .run_summary(SUMMARY_SYSTEM, "condense this".into())
            .await
            .unwrap();
        let n = cowboy_core::tokens::count(&got);
        assert!(
            n <= cap,
            "a summary must fit its allowance: {n} tokens against a cap of {cap}"
        );
        assert!(n > 0, "and it must not be emptied entirely");
        assert!(
            cowboy_core::tokens::count(&essay) > cap,
            "the fixture should exceed the cap, or this proves nothing"
        );
    }

    /// A window too small to hold the reserve leaves no room for a conversation, and
    /// there is nothing `fit_context` can trim to fix it. It used to return silently, so
    /// the request went out anyway and failed at the provider with a context-length
    /// error that names none of the numbers involved.
    #[tokio::test]
    async fn an_impossible_window_says_so_instead_of_failing_at_the_provider() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.context_window = 500;
        assert_eq!(agent.context_budget(), 0);
        agent.messages.push(Message::user("do something"));
        agent.fit_context().await;
        agent.fit_context().await; // twice: the notice must not repeat

        let hits: Vec<&String> = ui
            .notices
            .iter()
            .filter(|n| n.contains("too small for this model's max_tokens"))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "said once, with the numbers: {:?}",
            ui.notices
        );
        assert!(hits[0].contains("500"), "names the window: {}", hits[0]);
        assert!(
            hits[0].contains("models.yaml"),
            "points at the fix: {}",
            hits[0]
        );
    }

    /// When the pinned head (system prompt + task) alone exceeds the budget, no fold
    /// or drop can get under it. `fit_context` runs every turn, so it must NOT
    /// re-summarize the middle each time — that was ~100 wasted model calls. It warns
    /// once and leaves the history alone. (M4)
    #[tokio::test]
    async fn an_irreducible_pinned_head_does_not_spin_on_compaction() {
        let mut ui = RecordingUi::default();
        // A summarizer whose calls we can count: `summarize` uses the minimal-
        // reasoning client, which bumps `low_effort_calls`. If compaction tried to
        // fold, this would be > 0.
        let model = ScriptedModel::new(vec![]);
        let summary_calls = model.low_effort_calls.clone();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        // A huge system prompt + task so the pinned head alone blows a tiny budget.
        agent.messages = vec![
            Message::system("SYSTEM ".repeat(2000)),
            Message::user("TASK ".repeat(2000)),
        ];
        agent.task = Some(agent.messages[1].content.clone());
        // Some middle history that a naive fold would keep re-summarizing.
        for i in 0..6 {
            agent.push_tool_result(&format!("c{i}"), &"data ".repeat(50));
        }
        set_context_budget(&mut agent, 50); // far smaller than the pinned head

        let before = agent.messages.clone();
        agent.fit_context().await;
        agent.fit_context().await;
        agent.fit_context().await;

        assert_eq!(
            *summary_calls.lock().unwrap(),
            0,
            "an irreducible head must not trigger any summarization calls"
        );
        // Capture agent-derived facts before borrowing `ui` (agent holds `&mut ui`).
        let messages_unchanged = agent.messages == before;
        assert!(
            messages_unchanged,
            "nothing foldable/droppable, so the history is left intact"
        );
        let hits: Vec<&String> = ui
            .notices
            .iter()
            .filter(|n| n.contains("cannot compact further"))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "warned exactly once, not every turn: {:?}",
            ui.notices
        );
    }

    #[tokio::test]
    async fn tool_results_are_capped_however_they_were_produced() {
        let mut ui = RecordingUi::default();
        let cap = 500usize;
        let behavior = cowboy_core::config::AgentBehavior {
            max_command_output_bytes: cap,
            ..Default::default()
        };
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            behavior,
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let huge = "x".repeat(50_000);
        agent.push_tool_result("c1", &huge);
        let pushed = agent.messages.last().unwrap();
        assert_eq!(pushed.role, Role::Tool);
        assert!(
            pushed.content.len() < huge.len(),
            "a 50 KB result must not enter the context whole"
        );
        assert!(
            pushed.content.contains("truncated"),
            "the model should be told the result was cut: {}",
            pushed.content
        );
        // Short results are untouched — the cap must not mangle ordinary output.
        agent.push_tool_result("c2", "small result");
        assert_eq!(agent.messages.last().unwrap().content, "small result");
    }

    /// A subagent returning a huge answer is the worst case, because N of them land in
    /// one turn. Drives the real dispatch arm through a scripted subagent tool call.
    #[tokio::test]
    async fn a_huge_subagent_result_is_capped_in_the_foremans_context() {
        let mut ui = RecordingUi::default();
        let cap = 800usize;
        let behavior = cowboy_core::config::AgentBehavior {
            max_command_output_bytes: cap,
            ..Default::default()
        };
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            behavior,
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        // Stand in for what `run_subagents` collected, then let the arm fold it in.
        let giant = "subagent said a lot. ".repeat(5_000);
        agent.push_tool_result("sub-1", &giant);
        let folded = agent.messages.last().unwrap();
        assert!(
            folded.content.len() <= cap + 100,
            "expected ~{cap} bytes, got {}",
            folded.content.len()
        );
    }

    #[tokio::test]
    async fn the_context_budget_accounts_for_the_tool_schemas() {
        let mut ui = RecordingUi::default();
        let agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let schemas = agent.tools_tokens();
        assert!(
            schemas > 1000,
            "the real tool surface is thousands of tokens, got {schemas}"
        );
        // reserve = output budget + schemas + floor
        let reserve = 200_000 - agent.context_budget();
        assert_eq!(
            reserve,
            agent.model.max_output_tokens() + schemas + RESPONSE_HEADROOM
        );
        // Cached, so the loop is not re-tokenizing schemas every iteration.
        assert_eq!(agent.tools_tokens(), schemas);
    }

    /// A small-window model was the case the old formula got wrong: budget went
    /// negative-in-effect because the schemas were never subtracted. Saturating to
    /// zero makes `fit_context` bail rather than loop trying to fit the impossible.
    #[tokio::test]
    async fn a_window_too_small_for_the_schemas_yields_a_zero_budget() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.context_window = 1000;
        assert_eq!(agent.context_budget(), 0);
        // And fitting is a no-op rather than a panic or an infinite prune.
        agent.messages.push(Message::user("something"));
        let before = agent.messages.len();
        agent.fit_context().await;
        assert_eq!(agent.messages.len(), before);
    }

    /// Every shape a provider rejects, repaired in one pass — and normal history left
    /// untouched.
    ///
    /// The failure this guards is not turn-local: the history is replayed on every
    /// later call, so a single orphaned tool call 400s the session from then on. Rather
    /// than making each of `fit_context`, `compact_within_turn`, `drop_oldest` and
    /// `tail_within` individually responsible for not producing one, the invariant is
    /// enforced immediately before the model call.
    #[test]
    fn orphaned_tool_calls_and_results_are_repaired_before_the_model_sees_them() {
        let why = "not run";
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "shell".into(),
            arguments: "{}".into(),
        };
        let asst_with = |ids: &[&str]| {
            let mut m = Message::new(Role::Assistant, "");
            m.tool_calls = ids.iter().map(|i| call(i)).collect();
            m
        };

        // A well-formed conversation is not touched at all.
        let mut good = vec![
            Message::user("do it"),
            asst_with(&["a", "b"]),
            Message::tool_result("a", "ok"),
            Message::tool_result("b", "ok"),
            Message::new(Role::Assistant, "done"),
        ];
        let untouched = good.clone();
        assert_eq!(AgentLoop::enforce_tool_call_pairing(&mut good, why), 0);
        assert_eq!(good, untouched, "valid history must pass through unchanged");

        // An unanswered call gets a result, in place — not appended at the end, which
        // would put it after a later user turn.
        let mut dangling = vec![
            asst_with(&["a", "b"]),
            Message::tool_result("a", "ok"),
            Message::user("actually, do this instead"),
        ];
        assert_eq!(AgentLoop::enforce_tool_call_pairing(&mut dangling, why), 1);
        assert_eq!(dangling.len(), 4);
        assert_eq!(dangling[2].tool_call_id.as_deref(), Some("b"));
        assert_eq!(dangling[3].role, Role::User);

        // A result whose assistant turn was trimmed away has nothing to attach to.
        let mut orphan_result = vec![
            Message::tool_result("gone", "ok"),
            Message::user("carry on"),
        ];
        assert_eq!(
            AgentLoop::enforce_tool_call_pairing(&mut orphan_result, why),
            1
        );
        assert_eq!(orphan_result.len(), 1);
        assert_eq!(orphan_result[0].role, Role::User);

        // A result for an id this turn never asked for, and a duplicate answer to one
        // it did: both invalid, both dropped.
        let mut mismatched = vec![
            asst_with(&["a"]),
            Message::tool_result("a", "ok"),
            Message::tool_result("a", "ok again"),
            Message::tool_result("z", "from somewhere else"),
        ];
        assert_eq!(
            AgentLoop::enforce_tool_call_pairing(&mut mismatched, why),
            2
        );
        assert_eq!(mismatched.len(), 2);

        // Two dangling turns, not just the newest — the gap
        // `seal_dangling_tool_calls` leaves by design.
        let mut two = vec![asst_with(&["a"]), Message::user("hmm"), asst_with(&["b"])];
        assert_eq!(AgentLoop::enforce_tool_call_pairing(&mut two, why), 2);
        assert_eq!(two.len(), 5);
        assert_eq!(two[1].tool_call_id.as_deref(), Some("a"));
        assert_eq!(two[4].tool_call_id.as_deref(), Some("b"));

        // Idempotent: repairing a repaired history changes nothing.
        assert_eq!(AgentLoop::enforce_tool_call_pairing(&mut two, why), 0);
    }

    /// The invariant is wired into `call_model`, not merely available.
    ///
    /// Asserted by handing the loop a history no provider would accept and confirming
    /// that making a call repairs it. This is the wiring test; the shapes themselves
    /// are covered above.
    #[tokio::test]
    async fn calling_the_model_repairs_the_history_first() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![ChatResponse::default()])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let mut asst = Message::new(Role::Assistant, "");
        asst.tool_calls = vec![ToolCall {
            id: "a".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }];
        agent.messages = vec![
            Message::tool_result("trimmed-away", "orphan"),
            Message::user("go"),
            asst,
        ];

        let _ = agent.call_model().await;

        assert_eq!(
            AgentLoop::enforce_tool_call_pairing(&mut agent.messages, "x"),
            0,
            "the history must be valid after a call, not just before the next one"
        );
        assert!(
            !agent
                .messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("trimmed-away")),
            "the orphaned result must be gone"
        );
        assert_eq!(
            agent.messages.last().unwrap().tool_call_id.as_deref(),
            Some("a"),
            "and the dangling call must have been answered"
        );
    }

    /// Shrink `agent`'s window so the conversation budget is exactly `budget` tokens.
    ///
    /// Expressed relative to the loop's own reserve rather than as a magic number, so
    /// these tests keep testing pruning behaviour instead of breaking whenever the
    /// reserve formula or the tool surface changes size.
    fn set_context_budget(agent: &mut AgentLoop<'_>, budget: usize) {
        let reserve = agent.context_window - agent.context_budget();
        agent.context_window = reserve + budget;
        assert_eq!(agent.context_budget(), budget);
    }

    #[tokio::test]
    async fn fit_context_prunes_old_history_keeping_system() {
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        set_context_budget(&mut agent, 20); // tiny effective budget
        for i in 0..40 {
            agent.messages.push(Message::user(format!(
                "message number {i} with several words here"
            )));
        }
        let before = agent.messages.len();
        // No scripted summary -> summarization yields empty -> drop fallback.
        agent.fit_context().await;
        assert!(agent.messages.len() < before, "should have pruned");
        assert_eq!(agent.messages[0].role, Role::System, "system kept");
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("dropped") && n.contains("without summarizing")),
            "dropping history is lossy and should say so: {:?}",
            ui.notices
        );
    }

    #[tokio::test]
    async fn fit_context_compacts_old_turns_into_a_summary() {
        let mut ui = RecordingUi::default();
        // The model serves the compaction summary.
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: Some("SUMMARY: earlier turns did X and Y".into()),
            tool_calls: vec![],
        }]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        set_context_budget(&mut agent, 60);
        // Several whole turns (user -> assistant) so there are turn boundaries.
        for i in 0..12 {
            agent
                .messages
                .push(Message::user(format!("please do task {i} with detail")));
            agent
                .messages
                .push(Message::new(Role::Assistant, format!("did task {i} ok")));
        }
        agent.fit_context().await;

        // Folded into: [system, summary(system), recent turns…].
        assert_eq!(agent.messages[0].role, Role::System);
        assert_eq!(agent.messages[1].role, Role::System);
        assert!(agent.messages[1].content.contains("SUMMARY: earlier turns"));
        assert!(agent.messages[1]
            .content
            .contains("Summary of earlier conversation"));
        // The most recent turn is kept verbatim.
        let last = agent.messages.last().unwrap();
        assert!(last.content.contains("did task 11"));
        assert!(ui.notices.iter().any(|n| n.contains("compacted")));
    }

    #[tokio::test]
    async fn truncated_empty_turn_reports_incomplete_instead_of_silence() {
        // A reasoning model that burns its whole output budget thinking returns
        // no content and no tool call with finish_reason=length. Once recovery is
        // exhausted the loop must surface that explicitly (so a foreman reading a
        // subagent's stdout sees the cause) rather than returning an empty/None
        // result.
        //
        // Every turn truncates and returns no reasoning, so there is no salvage
        // summary to script: MAX_REPRIME_ATTEMPTS retries then the verdict.
        let mut ui = RecordingUi::default();
        let trunc = || ChatResponse {
            truncated: true,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![],
        };
        let model = ScriptedModel::new(
            std::iter::repeat_with(trunc)
                .take(MAX_REPRIME_ATTEMPTS as usize + 1)
                .collect(),
        );
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("review the diff").await.unwrap();
        let msg = res.expect("truncation should yield a descriptive result, not None");
        assert!(msg.starts_with("[incomplete]"), "got: {msg}");
        assert_eq!(classify_subagent_result(&msg), "error");
        // And it names the two levers the user actually has.
        assert!(
            msg.contains("max_tokens") && msg.contains("reasoning_effort"),
            "got: {msg}"
        );
    }

    /// The reported stall: a truncated turn with **no reasoning returned**.
    ///
    /// Plenty of providers bill reasoning tokens without ever sending the text, so
    /// there is nothing to distill — and the loop used to give up immediately with
    /// `[incomplete]`, which is what people saw: two notices about the output limit
    /// and a dead session. The model has the whole transcript and can simply be asked
    /// to finish, so recovery must not depend on salvage being possible.
    /// Reasoning is re-sent on every request, so accumulating it for the life of a
    /// session was the dominant growth term for a reasoning model. Only the most
    /// recent turns keep theirs; the rest is shed before the next call.
    #[tokio::test]
    async fn old_reasoning_is_shed_but_recent_turns_keep_theirs() {
        let mut ui = RecordingUi::default();
        // Four tool-using turns, each with reasoning, then a final answer.
        let thinking = |n: usize| ChatResponse {
            truncated: false,
            usage: None,
            reasoning: Some(format!("thinking about step {n} ").repeat(50)),
            content: None,
            tool_calls: vec![ToolCall {
                id: format!("c{n}"),
                name: "shell".into(),
                arguments: format!(r#"{{"command":"echo {n}"}}"#),
            }],
        };
        let model = ScriptedModel::new(vec![
            thinking(1),
            thinking(2),
            thinking(3),
            thinking(4),
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: Some("final thought".into()),
                content: Some("done".into()),
                tool_calls: vec![],
            },
        ]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("do the task").await.unwrap();
        assert_eq!(res.as_deref(), Some("done"));

        // The final turn is recorded after the last shed, so at most
        // REASONING_TURNS_KEPT + 1 messages still carry reasoning.
        let with_reasoning = agent
            .messages
            .iter()
            .filter(|m| m.reasoning.is_some())
            .count();
        assert!(
            with_reasoning <= REASONING_TURNS_KEPT + 1,
            "expected old reasoning to be shed, {with_reasoning} messages still carry it"
        );
        // And it is the *recent* ones that kept it, not arbitrary ones.
        let earliest_kept = agent
            .messages
            .iter()
            .position(|m| m.reasoning.is_some())
            .expect("some reasoning is kept");
        let assistant_count = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        assert!(assistant_count >= 4, "the run should have several turns");
        assert!(
            earliest_kept > agent.pinned(),
            "the kept reasoning should be recent, not at the head"
        );
        assert!(ui.notices.iter().any(|n| n.contains("older reasoning")));
    }

    /// Shedding must not touch the reasoning the next call actually needs: the turn
    /// whose tool result the model is about to read. Losing that is what makes an
    /// agentic reasoning model re-derive the same step and loop.
    #[tokio::test]
    async fn shedding_keeps_the_most_recent_reasoning_intact() {
        let mut ui = RecordingUi::default();
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: Some("ok".into()),
            tool_calls: vec![],
        }]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        // Three assistant turns with reasoning, oldest first.
        for n in 1..=3 {
            let mut m = Message::new(Role::Assistant, format!("turn {n}"));
            m.reasoning = Some(format!("reasoning {n}"));
            agent.messages.push(m);
        }
        let freed = agent.shed_reasoning();
        assert!(freed > 0, "the oldest reasoning should have been counted");
        let kept: Vec<Option<&str>> = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .map(|m| m.reasoning.as_deref())
            .collect();
        assert_eq!(
            kept,
            vec![None, Some("reasoning 2"), Some("reasoning 3")],
            "the two newest turns keep their reasoning, the oldest loses it"
        );
        // Idempotent: a second pass frees nothing and changes nothing.
        assert_eq!(agent.shed_reasoning(), 0);
    }

    #[tokio::test]
    async fn truncation_recovers_even_with_no_reasoning_to_salvage() {
        let mut ui = RecordingUi::default();
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: true,
                usage: None,
                reasoning: None, // provider billed the thinking but returned none
                content: None,
                tool_calls: vec![],
            },
            // No summary call is made (nothing to summarize), so the very next
            // response is the retry answering.
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("finished after the nudge".into()),
                tool_calls: vec![],
            },
        ]);
        let low_effort = model.low_effort_calls.clone();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("do the task").await.unwrap();
        assert_eq!(res.as_deref(), Some("finished after the nudge"));
        let nudged = agent.messages.iter().any(|m| {
            m.role == Role::User
                && m.content.contains("ran out of its output-token budget")
                && m.content.contains("Do NOT reason further")
        });
        assert!(nudged, "a retry directive should have been injected");
        // And the knob was actually turned: telling a reasoning model not to think is
        // advice it can ignore, which is how this stalled in the first place.
        assert_eq!(
            *low_effort.lock().unwrap(),
            1,
            "the retry must go out with minimal reasoning effort"
        );
        assert!(ui.notices.iter().any(|n| n.contains("recovering")));
    }

    /// Salvage that comes back empty must not sink the recovery either. The
    /// distillation runs on the same model that just spent its whole budget thinking,
    /// so an empty summary is the expected case, not an exotic one.
    #[tokio::test]
    async fn truncation_recovers_when_the_salvage_summary_is_empty() {
        let mut ui = RecordingUi::default();
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: true,
                usage: None,
                reasoning: Some("thinking at length".into()),
                content: None,
                tool_calls: vec![],
            },
            // The summary call itself yields nothing usable.
            ChatResponse {
                truncated: true,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("wrapped up anyway".into()),
                tool_calls: vec![],
            },
        ]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("do the task").await.unwrap();
        assert_eq!(res.as_deref(), Some("wrapped up anyway"));
        assert!(ui.notices.iter().any(|n| n.contains("recovering")));
    }

    /// The low-effort request is scoped to the retry. A model that answered is not the
    /// problem, and leaving its reasoning permanently dulled for the rest of the
    /// session would be a bad trade made invisibly.
    #[tokio::test]
    async fn minimal_reasoning_applies_only_to_the_retry_turn() {
        let mut ui = RecordingUi::default();
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: true,
                usage: None,
                reasoning: None,
                content: None,
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("recovered".into()),
                tool_calls: vec![],
            },
        ]);
        let low_effort = model.low_effort_calls.clone();
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("first task").await.unwrap();
        assert_eq!(*low_effort.lock().unwrap(), 1);

        // A second, healthy turn goes out at the configured effort.
        agent.model = Box::new(ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: Some("second answer".into()),
            tool_calls: vec![],
        }]));
        let res = agent.run("second task").await.unwrap();
        assert_eq!(res.as_deref(), Some("second answer"));
        assert_eq!(
            *low_effort.lock().unwrap(),
            1,
            "the override must not persist past the turn that needed it"
        );
    }

    #[tokio::test]
    async fn truncation_reprime_recovers_by_summarizing_reasoning() {
        // A turn truncates mid-thought (reasoning, no answer). The loop distills
        // the reasoning into a directive and retries, and the second turn wraps
        // up. Without a dedicated summarizer, the summary call falls back to the
        // main model — so the queue is: truncated, summary, final answer.
        let mut ui = RecordingUi::default();
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: true,
                usage: None,
                reasoning: Some("I should edit foo.rs and run the tests".into()),
                content: None,
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("conclusion: edit foo.rs, then test".into()),
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("all done".into()),
                tool_calls: vec![],
            },
        ]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("do the task").await.unwrap();
        assert_eq!(res.as_deref(), Some("all done"));
        // The distilled reasoning was injected as a directive to act now. (Inspect
        // `agent` before `ui`, which `agent` borrows mutably.)
        let injected = agent.messages.iter().any(|m| {
            m.role == Role::User
                && m.content.contains("already concluded")
                && m.content.contains("Do NOT reason further")
        });
        assert!(injected);
        // Warned about the output limit and announced the recovery.
        assert!(ui.notices.iter().any(|n| n.contains("output-token budget")));
        assert!(ui.notices.iter().any(|n| n.contains("recovering")));
    }

    #[tokio::test]
    async fn truncation_reprime_gives_up_after_cap() {
        // Every turn truncates. After MAX_REPRIME_ATTEMPTS recoveries the loop
        // stops with [incomplete] instead of spinning. Each attempt consumes a
        // truncated turn plus its (main-model) summary call.
        let mut ui = RecordingUi::default();
        let trunc = || ChatResponse {
            truncated: true,
            usage: None,
            reasoning: Some("still thinking hard".into()),
            content: None,
            tool_calls: vec![],
        };
        let summ = || ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: Some("concluded: keep going".into()),
            tool_calls: vec![],
        };
        let model = ScriptedModel::new(vec![
            trunc(),
            summ(), // attempt 1
            trunc(),
            summ(),  // attempt 2
            trunc(), // no attempts left -> [incomplete]
        ]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("do the task").await.unwrap();
        let msg = res.expect("should yield [incomplete], not None");
        assert!(msg.starts_with("[incomplete]"), "got: {msg}");
        // Reprime was attempted exactly MAX_REPRIME_ATTEMPTS times.
        let recoveries = ui
            .notices
            .iter()
            .filter(|n| n.contains("recovering"))
            .count();
        assert_eq!(recoveries, MAX_REPRIME_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn reprime_uses_the_dedicated_summarizer_model() {
        // With a summarizer configured, the distilled directive comes from IT, not
        // the main model — so the main model's queue only holds the truncated turn
        // and the final answer.
        let mut ui = RecordingUi::default();
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: true,
                usage: None,
                reasoning: Some("raw thinking".into()),
                content: None,
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
                usage: None,
                reasoning: None,
                content: Some("wrapped up".into()),
                tool_calls: vec![],
            },
        ]);
        let summarizer = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: Some("SUMMARIZER_SAYS: edit foo.rs".into()),
            tool_calls: vec![],
        }]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_summarizer(Some(Box::new(summarizer)));
        let res = agent.run("do the task").await.unwrap();
        assert_eq!(res.as_deref(), Some("wrapped up"));
        // The directive carries the summarizer's output, proving it was used.
        assert!(agent
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("SUMMARIZER_SAYS: edit foo.rs")));
    }

    #[test]
    fn unified_diff_renders_changes_and_caps_length() {
        let before = "fn a() {}\nfn b() {}\n";
        let after = "fn a() {}\nfn c() {}\n";
        let d = unified_diff("src/x.rs", before, after, 200);
        assert!(d.contains("--- a/src/x.rs"));
        assert!(d.contains("+++ b/src/x.rs"));
        assert!(d.contains("-fn b() {}"));
        assert!(d.contains("+fn c() {}"));

        // No change → empty.
        assert!(unified_diff("x", "same\n", "same\n", 200).is_empty());

        // Binary-looking content is skipped.
        assert!(unified_diff("x", "a", "b\u{0}c", 200).is_empty());

        // A huge change is capped with a marker.
        let big_before = String::new();
        let big_after = (0..500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let capped = unified_diff("x", &big_before, &big_after, 50);
        assert!(capped.lines().count() <= 51);
        assert!(capped.contains("more diff lines"));
    }

    #[test]
    fn truncate_keeps_short_output() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn truncate_cuts_long_output_on_boundary() {
        let big = "x".repeat(1000);
        let t = truncate(&big, 100);
        assert!(t.starts_with(&"x".repeat(100)));
        assert!(t.contains("truncated"));
    }

    /// The whole point: a build/test run that overruns the cap must still show its
    /// verdict. Head-only truncation hid it, so the model re-ran the command to find
    /// out what broke.
    #[test]
    fn middle_truncation_keeps_the_failure_summary_at_the_tail() {
        let mut out = String::from("[exit code: 101]\nerror[E0308]: mismatched types\n");
        for i in 0..5000 {
            out.push_str(&format!("   Compiling crate-{i} v0.1.0\n"));
        }
        out.push_str("test result: FAILED. 3 passed; 2 failed\n");

        let t = truncate_middle(&out, 4096);
        assert!(t.len() <= 4096, "must respect the cap: {}", t.len());
        // The head survives, including the caller's exit-code prefix…
        assert!(
            t.starts_with("[exit code: 101]\n"),
            "exit code must survive"
        );
        assert!(t.contains("error[E0308]"), "first error must survive");
        // …and so does the verdict, which head-only truncation threw away.
        assert!(
            t.contains("test result: FAILED. 3 passed; 2 failed"),
            "the tail carries the verdict and must survive"
        );
        assert!(t.contains("bytes elided"), "must say what it dropped");
        // Old behavior, for contrast: the verdict is gone.
        assert!(!truncate(&out, 4096).contains("test result: FAILED"));
    }

    #[test]
    fn middle_truncation_leaves_short_output_alone() {
        assert_eq!(truncate_middle("hello", 100), "hello");
        // Exactly at the cap is not truncation.
        let exact = "y".repeat(100);
        assert_eq!(truncate_middle(&exact, 100), exact);
    }

    /// No newline to snap to, and a cap too small to split, must still be safe and
    /// within budget — including on multibyte boundaries.
    #[test]
    fn middle_truncation_handles_degenerate_input() {
        // One enormous line: no line boundary anywhere.
        let minified = "a".repeat(10_000);
        let t = truncate_middle(&minified, 1000);
        assert!(t.len() <= 1000, "len {}", t.len());

        // Too small to split into two useful halves — falls back to head-only.
        let t = truncate_middle(&minified, 200);
        assert!(
            t.contains("output truncated at"),
            "should degrade to head: {t}"
        );

        // Multibyte characters must not be split.
        let wide = "日本語テキスト".repeat(500);
        let t = truncate_middle(&wide, 1024);
        assert!(t.len() <= 1024);
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
    }

    #[test]
    fn parse_args_handles_empty() {
        let a: FinalArgs = parse_args(r#"{"message":"done"}"#).unwrap();
        assert_eq!(a.message, "done");
    }

    #[test]
    fn stderr_tail_keeps_the_end_within_limits() {
        assert_eq!(stderr_tail(""), "");
        assert_eq!(stderr_tail("   \n  "), "");
        // Short stderr passes through (trimmed).
        assert_eq!(stderr_tail("boom: it failed\n"), "boom: it failed");
        // More than MAX_LINES keeps only the last lines (the error).
        let many = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = stderr_tail(&many);
        assert!(tail.contains("line 49"));
        assert!(!tail.contains("line 0\n"));
        assert!(tail.lines().count() <= 12);
        // Over the char cap is elided from the front but never panics on a
        // multibyte boundary.
        let big = format!("{}é-RESOURCE_EXHAUSTED", "x".repeat(5000));
        let tail = stderr_tail(&big);
        assert!(tail.starts_with('…'));
        assert!(tail.ends_with("RESOURCE_EXHAUSTED"));
    }

    #[test]
    fn signal_and_exit_failures_classify_as_error() {
        assert_eq!(
            classify_subagent_result("subagent error: killed by signal 9 (SIGKILL) — …"),
            "error"
        );
        assert_eq!(
            classify_subagent_result("subagent error: exited with status 1\nboom"),
            "error"
        );
    }

    #[test]
    fn partial_result_is_classified_as_error() {
        // A salvaged checkpoint isn't a clean completion — crew history records it
        // as a non-success so the route's success rate stays honest.
        assert_eq!(
            classify_subagent_result("[partial] did not finish; work so far…"),
            "error"
        );
    }

    #[tokio::test]
    async fn subagent_without_final_salvages_partial_work() {
        // A subagent whose turn ends without a clean final must hand the foreman a
        // `[partial]` checkpoint (latest narration + plan progress) instead of an
        // empty result, so the work isn't discarded and can be resumed.
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.subagent_depth = 1; // running as a subagent
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "Found 2 real issues in the auth path.".into(),
            tool_call_id: None,
            tool_calls: vec![],
            reasoning: None,
        });
        agent.plan = vec![
            ("Review auth".into(), "done".into()),
            ("Review export".into(), "pending".into()),
        ];

        let partial = agent.build_partial_result().expect("salvageable work");
        assert!(partial.starts_with("[partial]"));
        assert!(partial.contains("Found 2 real issues"));
        assert!(partial.contains("[x] Review auth"));
        assert!(partial.contains("[ ] Review export"));
    }

    /// The checkpoint must lead with what the worker *produced*, not its plan.
    ///
    /// Observed on a real review: a worker told to wrap up spent its whole allowance
    /// doing the right things — wrote a 17.6 KB audit, published it as an artifact, wrote
    /// a handoff — and ran out one turn before `final`. The checkpoint reported only its
    /// stale plan, whose last line was "[ ] Write prioritized findings artifact" *for the
    /// artifact it had just published*. The foreman read that as "nothing got done" and
    /// re-ran the whole review. The outputs were on disk the entire time.
    #[tokio::test]
    async fn a_partial_reports_the_artifacts_the_worker_published() {
        let root = assert_fs::TempDir::new().unwrap();
        let logger =
            crate::session::SessionLogger::create_with_id(root.path(), "partial-artifacts")
                .unwrap();
        let dir = logger.dir().to_path_buf();

        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            FakeSandbox::new(),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        )
        .with_logger(Some(logger));
        agent.subagent_depth = 1;

        // What the worker really did, recorded host-side by the artifact/handoff tools.
        cowboy_core::artifact::add_in(
            &dir,
            "partial-artifacts",
            cowboy_core::artifact::ArtifactKind::Review,
            "Async/locking performance audit",
            "## Findings\nthe full 17KB report",
            None,
            cowboy_core::time::now_ms(),
        )
        .unwrap();
        std::fs::write(dir.join("handoff.md"), "# Handoff\nwhere I got to").unwrap();

        // …and the misleading self-report it left behind.
        agent.plan = vec![("Write findings artifact".into(), "pending".into())];

        let partial = agent.build_partial_result().expect("salvageable work");
        assert!(
            partial.contains("Already produced"),
            "the checkpoint must name the outputs: {partial}"
        );
        assert!(
            partial.contains("Async/locking performance audit"),
            "the published artifact must be named: {partial}"
        );
        assert!(
            partial.contains("handoff"),
            "the handoff must be mentioned: {partial}"
        );
        // The stale plan is still shown, but no longer as if it were authoritative.
        let produced_at = partial.find("Already produced").unwrap();
        let plan_at = partial.find("Plan progress").unwrap();
        assert!(
            produced_at < plan_at,
            "outputs must come before the plan that contradicts them: {partial}"
        );
        assert!(
            partial.contains("may lag what it did"),
            "the plan needs the caveat, or it reads as ground truth: {partial}"
        );
    }
    // -----------------------------------------------------------------------
    // request_path: the agent asks, the user decides
    // -----------------------------------------------------------------------

    /// A native sandbox whose grant store is a temp directory, plus the project root
    /// and the store guard (both must outlive the loop).
    fn native_for_grants() -> (
        crate::sandbox::native::NativeSandbox,
        assert_fs::TempDir,
        assert_fs::TempDir,
    ) {
        let project = assert_fs::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".cowboy")).unwrap();
        let store = assert_fs::TempDir::new().unwrap();
        let root = std::fs::canonicalize(project.path()).unwrap();
        let sandbox = crate::sandbox::native::NativeSandbox::new(
            root,
            SecurityConfig::default(),
            Box::new(crate::cmd::sandbox::RealHost),
            std::sync::Arc::new(cowboy_gateway::DenyAll),
        )
        .unwrap()
        .with_grants_dir(store.path().to_path_buf());
        (sandbox, project, store)
    }

    fn request_path_response(path: &std::path::Path, read_only: bool) -> ChatResponse {
        ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call(
                "1",
                "request_path",
                &serde_json::json!({
                    "path": path.to_str().unwrap(),
                    "reason": "the shared proto definitions the client imports",
                    "read_only": read_only,
                })
                .to_string(),
            )],
        }
    }

    fn finished(message: &str) -> ChatResponse {
        ChatResponse {
            truncated: false,
            usage: None,
            reasoning: None,
            content: None,
            tool_calls: vec![tool_call(
                "2",
                "final",
                &serde_json::json!({"message": message}).to_string(),
            )],
        }
    }

    /// The whole point: an approved request widens the *next* command's view.
    #[tokio::test]
    async fn an_approved_request_path_grants_the_path() {
        let wanted = assert_fs::TempDir::new().unwrap();
        let wanted_path = std::fs::canonicalize(wanted.path()).unwrap();
        let (sandbox, _project, _store) = native_for_grants();

        let model = ScriptedModel::new(vec![
            request_path_response(&wanted_path, true),
            finished("done"),
        ]);
        let mut ui = RecordingUi {
            ask_answer: Some("allow for this session".into()),
            ..Default::default()
        };
        {
            let mut agent = AgentLoop::new(
                Box::new(model),
                sandbox,
                cowboy_core::config::AgentBehavior::default(),
                200_000,
                CancellationToken::new(),
                &mut ui,
            );
            agent.run("read the protos").await.unwrap();
            let granted = agent.runtime.granted_paths();
            assert_eq!(granted.len(), 1, "the path should be granted: {granted:?}");
            assert_eq!(granted[0].0, wanted_path);
            assert!(granted[0].1, "read-only was what was asked for");
        }

        // The user must have been shown the resolved path and the agent's reason —
        // that is what they are deciding on.
        let asked = ui.asks.join("\n");
        assert!(
            asked.contains(wanted_path.to_str().unwrap()),
            "the prompt must name the path: {asked}"
        );
        assert!(
            asked.contains("proto definitions"),
            "the prompt must carry the agent's reason: {asked}"
        );
        assert!(
            asked.contains("read-only"),
            "the prompt must state the access being asked for: {asked}"
        );
    }

    /// Fail closed. An answer that is not an explicit approval — including the empty
    /// string a non-interactive or unattended session returns — grants nothing.
    #[tokio::test]
    async fn an_unanswered_or_denied_request_path_grants_nothing() {
        for answer in ["", "deny", "no", "maybe later"] {
            let wanted = assert_fs::TempDir::new().unwrap();
            let wanted_path = std::fs::canonicalize(wanted.path()).unwrap();
            let (sandbox, _project, _store) = native_for_grants();
            let model = ScriptedModel::new(vec![
                request_path_response(&wanted_path, true),
                finished("done"),
            ]);
            let mut ui = RecordingUi {
                ask_answer: Some(answer.into()),
                ..Default::default()
            };
            let mut agent = AgentLoop::new(
                Box::new(model),
                sandbox,
                cowboy_core::config::AgentBehavior::default(),
                200_000,
                CancellationToken::new(),
                &mut ui,
            );
            agent.run("read the protos").await.unwrap();
            assert!(
                agent.runtime.granted_paths().is_empty(),
                "answer {answer:?} must not widen the boundary"
            );
        }
    }

    /// The user's approval is not the control for credentials. The model chose the
    /// path and wrote the reason, so a plausible-sounding request for `~/.ssh` is
    /// exactly the attack — the denylist refuses it *after* approval.
    ///
    /// The home directory is faked and the store created for real on disk, so this
    /// asserts the same thing on every machine instead of quietly skipping wherever
    /// `~/.ssh` happens not to exist.
    #[tokio::test]
    async fn an_approved_request_for_credentials_is_still_refused() {
        let home = assert_fs::TempDir::new().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let ssh = std::fs::canonicalize(&ssh).unwrap();
        let fake_home = std::fs::canonicalize(home.path()).unwrap();

        let project = assert_fs::TempDir::new().unwrap();
        let root = std::fs::canonicalize(project.path()).unwrap();
        let store = assert_fs::TempDir::new().unwrap();
        let probe = cowboy_sandbox::probe::FakeHost::new()
            .with_home(&fake_home)
            .with_existing(["/usr", root.to_str().unwrap()]);
        let sandbox = crate::sandbox::native::NativeSandbox::new(
            root,
            SecurityConfig::default(),
            Box::new(probe),
            std::sync::Arc::new(cowboy_gateway::DenyAll),
        )
        .unwrap()
        .with_grants_dir(store.path().to_path_buf());

        let model = ScriptedModel::new(vec![request_path_response(&ssh, true), finished("done")]);
        let mut ui = RecordingUi {
            ask_answer: Some("allow and remember for this project".into()),
            ..Default::default()
        };
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        {
            agent.run("read my ssh key").await.unwrap();
            assert!(
                agent.runtime.granted_paths().is_empty(),
                "an approved credential path must still be refused"
            );
            assert!(
                crate::sandbox::grants::load_in(store.path(), agent.runtime.root()).is_empty(),
                "and nothing may be written down either"
            );
        }

        assert!(
            !ui.asks.is_empty(),
            "the request must actually have reached the user, or this proves nothing"
        );
        assert!(
            ui.notices.iter().any(|n| n.contains("refused")),
            "the refusal must be surfaced: {:?}",
            ui.notices
        );
    }

    /// A path that does not exist is an error the agent can act on, not a grant and
    /// not a prompt — there is nothing for the user to decide about.
    #[tokio::test]
    async fn requesting_a_nonexistent_path_reports_an_error_without_asking() {
        let (sandbox, _project, _store) = native_for_grants();
        let model = ScriptedModel::new(vec![
            request_path_response(std::path::Path::new("/nope/definitely/not/here"), true),
            finished("done"),
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            sandbox,
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("read that folder").await.unwrap();
        assert!(agent.runtime.granted_paths().is_empty());
        assert!(
            ui.asks.is_empty(),
            "the user should not be asked about a path that does not exist: {:?}",
            ui.asks
        );
    }
}
