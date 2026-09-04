//! The Cowboy-owned agent loop: model turn -> tool call -> observation ->
//! repeat, until `final`, `ask_user` is answered, or limits are hit. Cowboy
//! owns this lifecycle; no agent framework.

use anyhow::Result;
use cowboy_core::config::AgentBehavior;
use cowboy_core::model::{ChatResponse, Delta, Message, ModelClient, Role, ToolDef};
use tokio_util::sync::CancellationToken;

use super::tools::{
    self, ArtifactArgs, AskUserArgs, BlockedArgs, DecisionArgs, EditArgs, FinalArgs, HandoffArgs,
    McpArgs, MemoryArgs, PlanArgs, ProposeScopeChangeArgs, ReadArgs, RequestPathArgs, ShellArgs,
    SubagentArgs, WriteArgs,
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
    emit_delta, fileop_summary, parse_args, render_plan, render_transcript, self_exe,
    tool_signature, truncate, unified_diff,
};

/// Default agent system prompt (see plan §10.3).
pub const SYSTEM_PROMPT: &str = "\
You are Cowboy, an autonomous coding agent running inside a Docker container.

The project is mounted at /workspace. You may freely inspect, edit, build, test, \
and run code inside the container. Use `shell` for builds, tests, git, and other \
commands. For files, prefer the structured tools: `read` (with line numbers), \
`edit` (exact unique-string replacement), and `write` (create/overwrite) — they \
are more reliable and cheaper than `cat`/`sed`/heredocs.

Cowboy-specific helpers are CLIs you invoke through `shell`, e.g. `cowboy patch \
show` and `cowboy proc start <name>`. You do not need to ask before ordinary \
development actions inside the container.

Reusable skills may be available: run `cowboy skill list` to see them and \
`cowboy skill show <name>` to read a skill's instructions, then follow them \
(skills are discovered from `.cowboy/skills/` and `.claude/skills/`).

Project conventions may live in AGENTS.md (or CLAUDE.md) files, which are \
authoritative. Before working in an area, `read` the repo-root AGENTS.md and the \
nearest AGENTS.md on the path to the files you're touching (the nearest one \
wins). When you establish — or the user tells you — a durable project convention \
(build/test commands, style rules, layout), record it in the appropriate \
AGENTS.md with `edit`/`write` so it persists for everyone.

You also have a private cross-session `memory` (stored on the host, not the \
repo). The index of what you've saved is shown below when present; `recall` a \
full entry by name when it's relevant, and `save` concise facts or user \
preferences worth remembering next time (default scope \"project\"; use \
\"global\" for things true across projects). Keep project conventions in \
AGENTS.md, not memory.

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

/// The crew-foreman delegation guidance, appended to the system prompt only in
/// crew mode (a roster exists and delegation is enabled). In solo mode the
/// selected model does all the work itself, so this isn't shown and the
/// `subagent` tool isn't offered.
pub const FOREMAN_PROMPT: &str =
    "\n\nYou are the foreman of a crew. For focused, separable work, delegate it with the \
`subagent` tool instead of doing everything yourself: describe the work by \
`category` (the kind — exploration, tests, frontend, backend, docs, \
debugging, refactor, e2e, or general) and `effort` (tiny/small/medium/large/\
deep), with a `reason` and the `expected_artifact`. Do NOT pick a model — Cowboy \
routes each request to the right crew model. To run work in parallel, emit \
several `subagent` calls in one message. Named specialist agents may be defined \
under `.claude/agents/`/`.cowboy/agents/` (`cowboy agents list`); adopt one by \
passing `agent: <name>` to `subagent`. Delegate when work is scoped and separable \
(exploration, test-writing, an independent component, a review pass); do it \
yourself when the task is tiny, the hand-off costs more than the work, or it \
needs continuous coordination with your current state. Prefer small, well-scoped \
subagent tasks that return a concrete artifact. If a subagent result comes back \
prefixed `[partial]`, it ran but did not finish cleanly — the text is its work so \
far plus a session id. Treat that as a checkpoint: re-delegate continuing from \
what's there (pass the prior work as `context`) rather than starting the task over.";

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

/// Builds a model client by name (host-owned credentials in, built client out),
/// yielding the client, its context window, and its (input, output) per-1M-token
/// USD pricing. Used to reroute when a model turns out to be unavailable.
pub type ModelBuilder =
    Box<dyn Fn(&str) -> Result<(Box<dyn ModelClient>, usize, (Option<f64>, Option<f64>))>>;

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
    /// One-shot notice that the window cannot fit the reserve — a config problem, so
    /// repeating it every iteration would just bury the turn.
    zero_budget_warned: bool,
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
    cmd.arg(&plan.task)
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
        // the `subagent` tool; in solo mode the selected model works alone.
        let crew_on = crate::cmd::crew::crew_enabled();
        let subagent_depth = std::env::var("COWBOY_SUBAGENT_DEPTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut system = if crew_on {
            format!("{SYSTEM_PROMPT}{FOREMAN_PROMPT}")
        } else {
            SYSTEM_PROMPT.to_string()
        };
        // A worker spawned as a subagent gets extra guidance to stream large
        // outputs to a file rather than risk losing them to a truncated tool call.
        if subagent_depth > 0 {
            system.push_str(SUBAGENT_PROMPT);
        }
        let tools = if crew_on {
            tools::definitions()
        } else {
            tools::definitions()
                .into_iter()
                .filter(|t| t.name != tools::TOOL_SUBAGENT)
                .collect()
        };
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
            zero_budget_warned: false,
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

    /// Accumulate per-call token estimates (prompt sent + completion received)
    /// and report the running session total to the UI. Estimates use the local
    /// tokenizer, so they are provider-independent and roughly track billing.
    fn account_tokens(&mut self, prompt_est: u64, response: &ChatResponse) {
        self.tokens_in += prompt_est;
        let mut out =
            cowboy_core::tokens::count(response.content.as_deref().unwrap_or_default()) as u64;
        // Reasoning is billed as output and can dwarf the visible answer on the
        // reasoning models this targets; omitting it made spend/budget read far
        // below the truth.
        out += cowboy_core::tokens::count(response.reasoning.as_deref().unwrap_or_default()) as u64;
        for tc in &response.tool_calls {
            out += (cowboy_core::tokens::count(&tc.arguments)
                + cowboy_core::tokens::count(&tc.name)) as u64;
        }
        self.tokens_out += out;
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
            self.cost_usd =
                (self.tokens_in as f64 / 1e6) * pi + (self.tokens_out as f64 / 1e6) * po;
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
        price_in: Option<f64>,
        price_out: Option<f64>,
    ) {
        self.model = model;
        self.context_window = context_window;
        self.price_in = price_in;
        self.price_out = price_out;
    }

    /// Toggle plan mode. While on, `edit`/`write` are refused (the agent must
    /// propose a plan and wait for the user to approve). Used by `/plan` / `/go`.
    pub fn set_planning(&mut self, on: bool) {
        self.planning = on;
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
    /// `[partial]` checkpoint the foreman can resume from: the agent's latest
    /// substantive narration, its plan progress, and the session id (whose
    /// `.cowboy/sessions/<id>/` dir holds the full transcript, scratchpad,
    /// published artifacts, and commands for recovery). Returns `None` only when
    /// there is genuinely nothing to report.
    fn build_partial_result(&self) -> Option<String> {
        let mut sections: Vec<String> = Vec::new();

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
        // completed steps.
        if !self.plan.is_empty() {
            let mut lines = String::from("Plan progress:");
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
        }
        let user_msg = Message::user(task);
        if let Some(l) = &mut self.logger {
            l.log_message(&user_msg);
        }
        self.messages.push(user_msg);
        self.task = Some(task.to_string());

        for _ in 0..self.behavior.max_iterations {
            if self.cancel.is_cancelled() {
                self.ui.notice("interrupted");
                return Ok(None);
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
            let same_call = self.last_tool_sig.as_deref() == Some(sig.as_str());
            if same_call && !self.last_obs_changed {
                self.tool_repeat += 1;
            } else {
                self.tool_repeat = 0;
            }
            self.last_tool_sig = Some(sig);
            const LOOP_NUDGE_AT: u32 = 3;
            const LOOP_ABORT_AT: u32 = 6;
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
                self.ui
                    .notice("loop guard: repeated identical action — nudging a change of approach");
                for c in &response.tool_calls {
                    self.push_tool_result(&c.id, &format!(
                        "[loop guard] You have issued this exact command {reps}× and gotten the same \
                         result. STOP repeating it — take a different approach, or call `final` if \
                         the task is complete."
                    ));
                }
                continue;
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
            // The turn may have been cancelled while a tool ran (a cancelled shell
            // exits 130 rather than erroring, so the loop reaches here); seal before
            // the top-of-loop cancel check unwinds us.
            if self.cancel.is_cancelled() {
                self.seal_dangling_tool_calls("not run: the turn was interrupted");
            }
        }

        self.ui.notice(&format!(
            "reached max_iterations ({})",
            self.behavior.max_iterations
        ));
        Ok(None)
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
            Ok((client, cw, (price_in, price_out))) => {
                self.fallback_used = true;
                self.set_model(client, cw, price_in, price_out);
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
        // Pre-pass: a planner that delegates several subtasks in one turn gets
        // them run *concurrently* (the gateway is the real backpressure; we only
        // cap local fan-out). Results are keyed by call id and consumed in order
        // by the sequential loop below, so tool-result ordering is preserved.
        // Skipped entirely while planning: the pre-pass would otherwise spawn
        // workers (which DO edit files) before the plan-mode gate below ever runs.
        let sub_results = if self.planning {
            Default::default()
        } else {
            self.run_subagents(&response.tool_calls).await
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
                     make changes. Use `read`/`grep`-style tools to investigate.",
                );
                continue;
            }
            match call.name.as_str() {
                tools::TOOL_FINAL => {
                    let Some(args) = self.parse_or_report::<FinalArgs>(call) else {
                        continue;
                    };
                    if let Some(l) = &self.logger {
                        l.write_final(&args.message);
                    }
                    self.ui.final_message(&args.message);
                    // Answer this call and any the model batched after it. An
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
                    let truncated = truncate(&output, self.behavior.max_command_output_bytes);
                    let observation = format!("[exit code: {}]\n{}", result.exit_code, truncated);
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
                    self.run_fileop(&call.id, &payload).await?;
                }
                tools::TOOL_EDIT => {
                    let Some(args) = self.parse_or_report::<EditArgs>(call) else {
                        continue;
                    };
                    let before = self.read_workspace_file(&args.path);
                    let payload = serde_json::json!({
                        "op": "edit", "path": args.path,
                        "old": args.old, "new": args.new, "replace_all": args.replace_all,
                    });
                    let (exit, out) = self.run_fileop(&call.id, &payload).await?;
                    self.ui
                        .tool_use(&fileop_summary("edit", &args.path, exit, &out));
                    if exit == 0 {
                        self.emit_file_diff(&args.path, before.as_deref());
                    }
                }
                tools::TOOL_WRITE => {
                    let Some(args) = self.parse_or_report::<WriteArgs>(call) else {
                        continue;
                    };
                    let before = self.read_workspace_file(&args.path);
                    let payload = serde_json::json!({
                        "op": "write", "path": args.path, "content": args.content,
                    });
                    let (exit, out) = self.run_fileop(&call.id, &payload).await?;
                    self.ui
                        .tool_use(&fileop_summary("write", &args.path, exit, &out));
                    if exit == 0 {
                        self.emit_file_diff(&args.path, before.as_deref());
                    }
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
    fn emit_file_diff(&mut self, path: &str, before: Option<&str>) {
        let after = self.read_workspace_file(path).unwrap_or_default();
        let before = before.unwrap_or("");
        if before == after {
            return;
        }
        // Cap the rendered diff so a huge file rewrite doesn't flood the pane;
        // the full change is still in the session log / on disk.
        const MAX_DIFF_LINES: usize = 200;
        let diff = unified_diff(path, before, &after, MAX_DIFF_LINES);
        if !diff.is_empty() {
            self.ui.file_diff(path, &diff);
        }
    }

    async fn run_fileop(
        &mut self,
        call_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(i32, String)> {
        let outcome = self.runtime.fileop(&payload.to_string()).await;
        // A fileop can trigger container bring-up too (e.g. after an idle stop);
        // surface any status lines it queued, even though only after the fact.
        self.drain_runtime_status();
        let (result, output) = match outcome {
            Ok(v) => v,
            Err(e) => {
                self.push_tool_result(call_id, &format!("error: {e}"));
                return Ok((-1, String::new()));
            }
        };
        let observation = if result.exit_code == 0 {
            output.clone()
        } else {
            format!("error: {}", output.trim())
        };
        // `push_tool_result` applies the same cap; truncating here as well keeps the
        // `[exit code: N]` prefix outside the truncated region.
        let observation = truncate(&observation, self.behavior.max_command_output_bytes);
        self.push_tool_result(call_id, &observation);
        Ok((result.exit_code, output))
    }

    /// Run a shell command with live streaming to the UI (interruptible via the
    /// turn's cancel token). Returns (exit, full output).
    async fn run_shell_streaming(&mut self, args: &ShellArgs) -> Result<(ExecResult, String)> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let fut = self.runtime.exec_stream(
            &args.command,
            args.cwd.as_deref(),
            self.behavior.command_timeout_seconds,
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

    /// Plan every `subagent` call in this turn, announce them, then execute them
    /// concurrently (capped by `delegation.max_parallel`). Returns call id →
    /// result. Parse / depth errors become the result for that call.
    async fn run_subagents(
        &mut self,
        calls: &[cowboy_core::model::ToolCall],
    ) -> std::collections::HashMap<String, String> {
        use futures::stream::StreamExt;
        let mut results: std::collections::HashMap<String, String> = Default::default();
        let sub_calls: Vec<&cowboy_core::model::ToolCall> = calls
            .iter()
            .filter(|c| c.name == tools::TOOL_SUBAGENT)
            .collect();
        if sub_calls.is_empty() {
            return results;
        }
        let crew_cfg = cowboy_core::crew::load().ok().flatten();
        let max_parallel = crew_cfg
            .as_ref()
            .map(|c| c.delegation.max_parallel.max(1) as usize)
            .unwrap_or(4);

        // Plan + announce sequentially (needs &mut self); collect runnable plans.
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
        if plans.len() > 1 {
            self.ui
                .notice(&format!("↳ running {} subagents in parallel", plans.len()));
        }
        // Per-provider throttle: cap concurrent workers hitting the same provider
        // so a batch of same-model subagents can't trip its rate limit (429),
        // while different providers still run fully in parallel. Bounded further by
        // `max_parallel`; 0 = unlimited.
        let per_provider = crew_cfg
            .as_ref()
            .map(|c| c.delegation.max_parallel_per_provider)
            .unwrap_or(2) as usize;
        let model_defs = load_model_defs(self.root());
        let foreman = crate::cmd::crew::foreman_model();
        let mut provider_sems: std::collections::HashMap<
            String,
            std::sync::Arc<tokio::sync::Semaphore>,
        > = std::collections::HashMap::new();
        if per_provider > 0 {
            for (_, plan) in &plans {
                let key = provider_key(plan.model.as_deref(), &model_defs, foreman.as_deref());
                provider_sems.entry(key).or_insert_with(|| {
                    std::sync::Arc::new(tokio::sync::Semaphore::new(per_provider))
                });
            }
        }

        // Remember every dispatched call so an interrupt can synthesize results
        // for the ones still running (see the salvage arm below).
        let dispatched: Vec<(String, String, String)> = plans
            .iter()
            .map(|(id, plan)| {
                let label = plan
                    .label
                    .split(" → ")
                    .next()
                    .unwrap_or(&plan.label)
                    .to_string();
                (id.clone(), label, plan.id.clone())
            })
            .collect();

        // Execute concurrently (owned plans → no borrow of self), timing each and
        // capturing a coarse outcome for the crew history. Process completions as
        // they arrive so the background pane flips each subagent to done/failed
        // with its own elapsed time (rather than all at once at the end).
        let mut stream = futures::stream::iter(plans.into_iter().map(|(id, plan)| {
            let routed = plan.routed.clone();
            let label = plan
                .label
                .split(" → ")
                .next()
                .unwrap_or(&plan.label)
                .to_string();
            let sub_id = plan.id.clone();
            let key = provider_key(plan.model.as_deref(), &model_defs, foreman.as_deref());
            let sem = provider_sems.get(&key).cloned();
            async move {
                // Hold a provider permit for the worker's whole lifetime; `None`
                // means the throttle is disabled (unlimited).
                let _permit = match sem {
                    Some(s) => s.acquire_owned().await.ok(),
                    None => None,
                };
                let started = std::time::Instant::now();
                let result = exec_subagent(plan).await;
                let duration_ms = started.elapsed().as_millis() as u64;
                let status = classify_subagent_result(&result).to_string();
                let outcome = routed.map(|(category, effort, model, fell_back)| {
                    cowboy_core::crew::CrewOutcome {
                        ts_ms: now_ms(),
                        category,
                        effort,
                        model,
                        fell_back,
                        status: status.clone(),
                        duration_ms,
                    }
                });
                (id, label, sub_id, result, status, outcome)
            }
        }))
        .buffer_unordered(max_parallel);

        let root = self.root().to_path_buf();
        let cancel = self.cancel.clone();
        loop {
            let item = tokio::select! {
                biased;
                // Interrupted mid-batch: keep every completed subagent's result
                // (finished work must reach the transcript, or the foreman re-runs
                // it all from scratch next turn) and synthesize a checkpoint
                // marker for the rest. Dropping `stream` kills the still-running
                // children via kill_on_drop.
                _ = cancel.cancelled() => {
                    let mut stopped = 0usize;
                    for (id, label, sub_id) in &dispatched {
                        if results.contains_key(id) {
                            continue;
                        }
                        stopped += 1;
                        self.ui.subagent_done(label, false, sub_id);
                        results.insert(
                            id.clone(),
                            format!(
                                "[interrupted] this subagent was stopped before it \
                                 finished; whatever it completed (transcript, \
                                 scratchpad, commands) is in .cowboy/sessions/{sub_id}/ \
                                 — resume from that checkpoint rather than redoing \
                                 work, and do NOT re-run subagents that returned \
                                 results above"
                            ),
                        );
                    }
                    self.ui.notice(&format!(
                        "interrupted — kept {} finished subagent result(s); \
                         stopped {stopped} still running",
                        dispatched.len() - stopped
                    ));
                    break;
                }
                item = stream.next() => item,
            };
            let Some((id, label, sub_id, res, status, outcome)) = item else {
                self.ui.notice("↳ subagent(s) finished");
                break;
            };
            self.ui.subagent_done(&label, status == "complete", &sub_id);
            if let Some(o) = outcome {
                cowboy_core::crew::record_outcome(&o);
            }
            // Roll the finished subagent's spend into the session total so the UI
            // reflects delegated work (subagents run as separate processes; their
            // cost would otherwise be invisible). Reported as each child lands.
            let usage = read_subagent_usage(&root, &sub_id);
            self.subagent_cost_usd += usage.cost_usd;
            self.subagent_tokens_in += usage.tokens_in;
            self.subagent_tokens_out += usage.tokens_out;
            self.report_usage();
            results.insert(id, res);
        }
        results
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

        let max_depth = match crew_cfg {
            Some(c) if !c.delegation.allow_recursive_delegation => c.delegation.max_depth as usize,
            _ => MAX_SUBAGENT_DEPTH,
        }
        .min(MAX_SUBAGENT_DEPTH);
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
        self.ui.subagent_started(
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
            Ok(self.record(payload))
        }
        async fn stop_all_processes(&self) -> Result<()> {
            Ok(())
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
                reasoning: None,
                content: Some("inspecting".into()),
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"ls"}"#)],
            },
            ChatResponse {
                truncated: false,
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
    async fn stops_when_token_budget_reached_and_reports_cost() {
        // The model keeps asking for shell (never finals); only the budget stops it.
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                reasoning: None,
                content: Some("working".into()),
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"ls"}"#)],
            },
            ChatResponse {
                truncated: false,
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
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "unblock", "{}")],
            },
            ChatResponse {
                truncated: false,
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

    /// Plan mode is HOST-enforced: while planning, the agent must not be able to
    /// mutate the workspace via `shell`, nor escape the gate by delegating to a
    /// subagent (which is not itself in plan mode).
    #[tokio::test]
    async fn plan_mode_blocks_shell_and_subagent_not_just_edits() {
        // The gate must stop the command before it ever reaches the container.
        let m = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
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
                    reasoning: None,
                    content: None,
                    tool_calls: vec![tool_call("f", "final", r#"{"message":"rescued"}"#)],
                }]);
                Ok((Box::new(m) as Box<dyn ModelClient>, 200_000, (None, None)))
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
    async fn runs_edit_via_fileop_then_final() {
        let sandbox = FakeSandbox::printing("edited main.rs: 1 replacement\n");
        let fileops = sandbox.log();
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
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
        assert_eq!(ui.tool_uses, vec!["edited main.rs: 1 replacement"]);
        // And the edit really went through the structured file-op path, carrying the
        // op and the path — not through a shell command.
        let sent = fileops.lock().unwrap().join("\n");
        assert!(
            sent.contains("\"op\":\"edit\"") && sent.contains("main.rs"),
            "the edit should reach the file-op helper: {sent}"
        );
    }

    #[tokio::test]
    async fn plan_mode_blocks_edits_until_approved() {
        // The sandbox records everything it was asked to run, and the assertion at
        // the end is that it was asked for *nothing* — so this checks the gate
        // actually prevents the mutation rather than merely discouraging it.
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
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
        let mut ui = RecordingUi::default();
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
        assert!(ui.notices.iter().any(|n| n.contains("max_iterations")));
        assert_eq!(ui.commands.len(), 3);
    }

    #[tokio::test]
    async fn multi_turn_retains_conversation_context() {
        // Two turns on the same loop; the conversation must accumulate.
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "final", r#"{"message":"done 1"}"#)],
            },
            ChatResponse {
                truncated: false,
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

    #[tokio::test]
    async fn run_subagents_batches_results_by_call_id() {
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
        let results = agent.run_subagents(&calls).await;
        assert_eq!(results.len(), 2, "only subagent calls produce results");
        assert!(results.contains_key("a") && results.contains_key("b"));
        assert!(!results.contains_key("c"));
        assert!(results["a"].contains("depth limit"));
    }

    #[tokio::test]
    async fn interrupted_subagent_batch_salvages_results_instead_of_dropping_them() {
        // An interrupt mid-batch must not throw away the batch: every dispatched
        // call still gets a tool result (here a checkpoint marker, since nothing
        // finished — the biased select sees the pre-cancelled token before the
        // stream is first polled, so no child process ever spawns). Without this,
        // the transcript ends the turn with dangling subagent calls and the
        // foreman re-runs everything from scratch next turn.
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
        let calls = vec![tool_call(
            "a",
            "subagent",
            r#"{"task":"x","category":"tests","effort":"small"}"#,
        )];
        let results = agent.run_subagents(&calls).await;
        let res = results
            .get("a")
            .expect("interrupted call still gets a result");
        assert!(
            res.contains("[interrupted]") && res.contains(".cowboy/sessions/"),
            "should point at the child's checkpoint, got: {res}"
        );
        assert!(
            ui.notices
                .iter()
                .any(|n| n.contains("interrupted — kept 0 finished subagent result(s)")),
            "salvage notice should report what was kept/stopped: {:?}",
            ui.notices
        );
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

    /// tiktoken is slow enough that repeated full passes dominate the loop's own cost:
    /// measured ~570ms for one pass over a 300-message / 110k-token conversation, and
    /// the loop makes several passes per iteration. This asserts the memo actually
    /// bites, rather than trusting that it does.
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
        let t = std::time::Instant::now();
        let cold_total = agent.total_tokens();
        let cold = t.elapsed();
        let t = std::time::Instant::now();
        let warm_total = agent.total_tokens();
        let warm = t.elapsed();

        assert_eq!(
            cold_total, warm_total,
            "the memo must not change the answer"
        );
        assert!(cold_total > 1000, "the fixture should be substantial");
        assert!(
            warm * 5 < cold,
            "the second pass should be far cheaper: cold {cold:?} vs warm {warm:?}"
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
                reasoning: None, // provider billed the thinking but returned none
                content: None,
                tool_calls: vec![],
            },
            // No summary call is made (nothing to summarize), so the very next
            // response is the retry answering.
            ChatResponse {
                truncated: false,
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
                reasoning: Some("thinking at length".into()),
                content: None,
                tool_calls: vec![],
            },
            // The summary call itself yields nothing usable.
            ChatResponse {
                truncated: true,
                reasoning: None,
                content: None,
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
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
                reasoning: None,
                content: None,
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
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
                reasoning: Some("I should edit foo.rs and run the tests".into()),
                content: None,
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
                reasoning: None,
                content: Some("conclusion: edit foo.rs, then test".into()),
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
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
            reasoning: Some("still thinking hard".into()),
            content: None,
            tool_calls: vec![],
        };
        let summ = || ChatResponse {
            truncated: false,
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
                reasoning: Some("raw thinking".into()),
                content: None,
                tool_calls: vec![],
            },
            ChatResponse {
                truncated: false,
                reasoning: None,
                content: Some("wrapped up".into()),
                tool_calls: vec![],
            },
        ]);
        let summarizer = ScriptedModel::new(vec![ChatResponse {
            truncated: false,
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
