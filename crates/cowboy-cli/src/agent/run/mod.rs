//! The Cowboy-owned agent loop: model turn -> tool call -> observation ->
//! repeat, until `final`, `ask_user` is answered, or limits are hit. Cowboy
//! owns this lifecycle; no agent framework.

use anyhow::Result;
use cowboy_core::config::AgentBehavior;
use cowboy_core::model::{ChatResponse, Delta, Message, ModelClient, Role, ToolDef};
use tokio_util::sync::CancellationToken;

use super::tools::{
    self, ArtifactArgs, AskUserArgs, BlockedArgs, DecisionArgs, EditArgs, FinalArgs, HandoffArgs,
    McpArgs, MemoryArgs, PlanArgs, ProposeRanchArgs, ProposeScopeChangeArgs, ReadArgs, ShellArgs,
    SubagentArgs, WriteArgs,
};
use super::ui::AgentUi;
use crate::net::docker::ExecResult;
use crate::net::runtime::AgentRuntime;
use crate::session::SessionLogger;

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

/// Drives a single agent session.
pub struct AgentLoop<'a> {
    model: Box<dyn ModelClient>,
    runtime: AgentRuntime,
    tools: Vec<ToolDef>,
    behavior: AgentBehavior,
    cancel: CancellationToken,
    /// Model context window (tokens) for history pruning.
    context_window: usize,
    pruned_notified: bool,
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
    cost_usd: f64,
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
    tool_repeat: u32,
    /// Plan mode: while on, file-mutating tools (`edit`/`write`) are refused so
    /// the agent proposes a plan and waits for the user to approve (`/go`). Host-
    /// enforced — the agent can't edit during planning even if it tries.
    planning: bool,
    /// Connected MCP servers for this session (host-side). `None` when no servers
    /// are enabled; set via [`AgentLoop::enable_mcp`], which also adds the `mcp`
    /// tool and lists the servers in the system prompt.
    mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    messages: Vec<Message>,
    ui: &'a mut dyn AgentUi,
    logger: Option<SessionLogger>,
}

/// A planned subagent delegation, ready to execute. Owns everything it needs so
/// a batch can run concurrently without borrowing the parent loop.
#[derive(Debug)]
struct SubagentPlan {
    exe: std::path::PathBuf,
    root: std::path::PathBuf,
    container_name: String,
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

/// Execute one planned subagent: a nested one-shot `cowboy` run sharing this
/// session's container. No parent borrow, so many can run concurrently.
async fn exec_subagent(plan: SubagentPlan) -> String {
    use std::os::unix::process::ExitStatusExt;
    let mut cmd = tokio::process::Command::new(&plan.exe);
    cmd.arg(&plan.task)
        .current_dir(&plan.root)
        .env("COWBOY_CONTAINER_NAME", &plan.container_name)
        .env("COWBOY_SUBAGENT_DEPTH", plan.child_depth.to_string())
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

/// Tokens reserved for the model's response + tool schemas when budgeting.
const RESPONSE_HEADROOM: usize = 4096;
/// Maximum subagent nesting depth (prevents runaway recursion).
const MAX_SUBAGENT_DEPTH: usize = 2;

impl<'a> AgentLoop<'a> {
    pub fn new(
        model: Box<dyn ModelClient>,
        runtime: AgentRuntime,
        behavior: AgentBehavior,
        context_window: usize,
        cancel: CancellationToken,
        ui: &'a mut dyn AgentUi,
    ) -> Self {
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
            runtime,
            tools,
            behavior,
            cancel,
            context_window,
            pruned_notified: false,
            subagent_depth,
            last_final: None,
            tokens_in: 0,
            tokens_out: 0,
            price_in: None,
            price_out: None,
            cost_usd: 0.0,
            budget_warned: false,
            plan: Vec::new(),
            lifecycle_started: false,
            setup_done: false,
            last_tool_sig: None,
            tool_repeat: 0,
            planning: false,
            mcp: None,
            messages: vec![Message::system(system)],
            ui,
            logger: None,
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
        for tc in &response.tool_calls {
            out += (cowboy_core::tokens::count(&tc.arguments)
                + cowboy_core::tokens::count(&tc.name)) as u64;
        }
        self.tokens_out += out;
        self.ui.tokens(self.tokens_in, self.tokens_out);
        if let (Some(pi), Some(po)) = (self.price_in, self.price_out) {
            self.cost_usd =
                (self.tokens_in as f64 / 1e6) * pi + (self.tokens_out as f64 / 1e6) * po;
            self.ui.cost(self.cost_usd);
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
    fn budget_reached(&self) -> Option<String> {
        let b = &self.behavior;
        let used = self.tokens_in + self.tokens_out;
        if b.token_budget > 0 && used >= b.token_budget {
            return Some(format!(
                "token budget reached ({used} tokens ≥ {}); stopping",
                b.token_budget
            ));
        }
        if b.cost_budget_usd > 0.0 && self.cost_usd >= b.cost_budget_usd {
            return Some(format!(
                "cost budget reached (${:.2} ≥ ${:.2}); stopping",
                self.cost_usd, b.cost_budget_usd
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
        let used = self.tokens_in + self.tokens_out;
        let warn = if b.token_budget > 0 && used as f64 >= 0.8 * b.token_budget as f64 {
            Some(format!(
                "approaching token budget ({used}/{} tokens)",
                b.token_budget
            ))
        } else if b.cost_budget_usd > 0.0 && self.cost_usd >= 0.8 * b.cost_budget_usd {
            Some(format!(
                "approaching cost budget (${:.2}/${:.2})",
                self.cost_usd, b.cost_budget_usd
            ))
        } else {
            None
        };
        if let Some(w) = warn {
            self.ui.notice(&w);
            self.budget_warned = true;
        }
    }

    /// Approximate token count of a message (content + tool-call arguments).
    fn message_tokens(m: &Message) -> usize {
        let mut n = cowboy_core::tokens::count(&m.content) + 4;
        for tc in &m.tool_calls {
            n += cowboy_core::tokens::count(&tc.arguments)
                + cowboy_core::tokens::count(&tc.name)
                + 4;
        }
        n
    }

    /// Total estimated tokens of the current conversation.
    fn total_tokens(&self) -> usize {
        self.messages.iter().map(Self::message_tokens).sum()
    }

    /// Keep the conversation within the context window. When it overflows, fold
    /// the oldest whole turns into a single model-generated summary message
    /// rather than dropping them, so earlier decisions, edits, and facts survive.
    /// Compaction happens at user-turn boundaries (turn starts) so a tool result
    /// is never orphaned. Falls back to dropping if a summary can't be made.
    async fn fit_context(&mut self) {
        // Reserve room for the response: the model's own max output tokens (so
        // prompt + output never exceeds the window), with RESPONSE_HEADROOM as a
        // floor that also covers tool-schema overhead.
        let reserve = self.model.max_output_tokens().max(RESPONSE_HEADROOM);
        let budget = self.context_window.saturating_sub(reserve);
        if budget == 0 || self.total_tokens() <= budget {
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
        for &idx in user_idxs.iter().rev() {
            let tail: usize = self.messages[idx..].iter().map(Self::message_tokens).sum();
            if tail <= tail_budget {
                keep_from = idx;
            } else {
                break;
            }
        }
        // Nothing before the kept tail to summarize (e.g. one huge turn): drop.
        if keep_from <= 1 {
            self.drop_oldest(budget);
            return;
        }

        let old: Vec<Message> = self.messages[1..keep_from].to_vec();
        let folded = old.len();
        let summary = match self.summarize(&old).await {
            Ok(s) if !s.trim().is_empty() => s,
            _ => {
                self.drop_oldest(budget);
                return;
            }
        };
        let mut rebuilt = Vec::with_capacity(self.messages.len() - folded + 1);
        rebuilt.push(self.messages[0].clone());
        rebuilt.push(Message::system(format!(
            "[Summary of earlier conversation, compacted to save context]\n{summary}"
        )));
        rebuilt.extend_from_slice(&self.messages[keep_from..]);
        self.messages = rebuilt;
        self.ui.notice(&format!(
            "compacted {folded} earlier messages into a summary"
        ));
    }

    /// Ask the model to summarize a span of prior messages into a dense brief.
    async fn summarize(&self, old: &[Message]) -> Result<String> {
        let msgs = vec![
            Message::system(SUMMARY_SYSTEM),
            Message::user(format!(
                "{}\n\n---\nWrite the summary now.",
                render_transcript(old)
            )),
        ];
        let resp = self.model.chat(&msgs, &[], None).await?;
        Ok(resp.content.unwrap_or_default())
    }

    /// Last-resort pruning: drop the oldest messages (never the system message),
    /// skipping orphaned tool results, until within budget.
    fn drop_oldest(&mut self, budget: usize) {
        let mut pruned = false;
        while self.messages.len() > 2 && self.total_tokens() > budget {
            self.messages.remove(1);
            while self.messages.len() > 1 && self.messages[1].role == Role::Tool {
                self.messages.remove(1);
            }
            pruned = true;
        }
        if pruned && !self.pruned_notified {
            self.pruned_notified = true;
            self.ui.notice("context window full; pruned older history");
        }
    }

    /// Attach a session logger (records transcript, commands, final summary).
    pub fn with_logger(mut self, logger: Option<SessionLogger>) -> Self {
        self.logger = logger;
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
        // Insert after messages[0] (system), preserving order, before any task.
        for (i, m) in history.into_iter().enumerate() {
            self.messages.insert(1 + i, m);
        }
        self
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

    /// One-time per-session setup, run before the first turn while the UI is
    /// live. When the workspace uses mise, run a *visible* `mise install` (it
    /// streams to the transcript with the live indicator) so installing the
    /// project toolchain doesn't look like a hung first request. Best-effort:
    /// a failure surfaces its exit code but doesn't block the session.
    async fn run_session_setup(&mut self) {
        if self.setup_done {
            return;
        }
        self.setup_done = true;
        // Subagents share the parent's container/toolchain — only the top-level
        // session does setup.
        if self.subagent_depth > 0 || !self.runtime.has_mise_config() {
            return;
        }
        self.ui
            .notice("setting up project toolchain (mise install)…");
        let args = ShellArgs {
            command: "mise install".to_string(),
            cwd: None,
        };
        self.ui.command_start(&args.command);
        match self.run_shell_streaming(&args).await {
            Ok((result, _)) => self.ui.command_end(result.exit_code, ""),
            Err(e) => self.ui.notice(&format!("mise install did not run: {e}")),
        }
    }

    /// Finalize the session log (diff + summary). Call once when the
    /// conversation ends.
    pub fn finalize_session(&self) {
        let status = if self.last_final.is_some() {
            "complete"
        } else {
            "incomplete"
        };
        self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::SessionCompleted {
            status: status.to_string(),
        });
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

            // Keep history within the model's context window.
            self.fit_context().await;

            // Estimate the prompt tokens actually sent (post-pruning).
            let prompt_est: u64 = self
                .messages
                .iter()
                .map(Self::message_tokens)
                .sum::<usize>() as u64;
            let response = match self.call_model().await {
                Ok(r) => r,
                Err(_) if self.cancel.is_cancelled() => {
                    self.ui.notice("interrupted");
                    return Ok(None);
                }
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
                let msg = response.content.unwrap_or_default();
                if !msg.is_empty() {
                    self.ui.final_message(&msg);
                    return Ok(Some(msg));
                }
                // Truncated mid-generation with nothing usable: a reasoning model
                // can spend its entire output budget thinking and never emit an
                // answer or tool call. Report it explicitly so the caller (a
                // foreman reading a subagent's stdout, or the user) sees the
                // cause instead of a silent empty result.
                if response.truncated {
                    let note = "model hit its output-token limit while reasoning \
                                and produced no answer (no content, no tool call)";
                    self.ui.notice(note);
                    return Ok(Some(format!("[incomplete] {note}")));
                }
                self.ui.notice(
                    "the model didn't return anything to do — rephrase your request, \
                     or try a different model with /model",
                );
                return Ok(None);
            }

            // Loop guard: re-issuing the identical tool call(s) yields the same
            // result and makes no progress (a degenerate model loop). Nudge after
            // a few repeats, abort if it persists — so a runaway costs seconds,
            // not a hundred API calls.
            let sig = tool_signature(&response.tool_calls);
            if self.last_tool_sig.as_deref() == Some(sig.as_str()) {
                self.tool_repeat += 1;
            } else {
                self.tool_repeat = 0;
                self.last_tool_sig = Some(sig);
            }
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

            if let Some(final_msg) = self.handle_tool_calls(&response).await? {
                return Ok(Some(final_msg));
            }
        }

        self.ui.notice(&format!(
            "reached max_iterations ({})",
            self.behavior.max_iterations
        ));
        Ok(None)
    }

    /// Push a tool-result message (logged + added to history).
    fn push_tool_result(&mut self, tool_call_id: &str, content: &str) {
        let msg = Message::tool_result(tool_call_id, content);
        if let Some(l) = &mut self.logger {
            l.log_message(&msg);
        }
        self.messages.push(msg);
    }

    /// Process this turn's tool calls. Returns `Some(message)` if `final` was
    /// called.
    async fn handle_tool_calls(&mut self, response: &ChatResponse) -> Result<Option<String>> {
        // Pre-pass: a planner that delegates several subtasks in one turn gets
        // them run *concurrently* (the gateway is the real backpressure; we only
        // cap local fan-out). Results are keyed by call id and consumed in order
        // by the sequential loop below, so tool-result ordering is preserved.
        let sub_results = self.run_subagents(&response.tool_calls).await;

        for call in &response.tool_calls {
            // Plan mode gate: refuse file-mutating tools so the agent proposes a
            // plan instead of editing. Host-enforced — independent of the prompt.
            if self.planning && matches!(call.name.as_str(), tools::TOOL_EDIT | tools::TOOL_WRITE) {
                self.push_tool_result(
                    &call.id,
                    "blocked: plan mode is on — do not modify files yet. Present your plan \
                     (use the `plan` tool to list the steps), then stop; the user will \
                     approve with /go before you make changes.",
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
                    return Ok(Some(args.message));
                }
                tools::TOOL_SHELL => {
                    let Some(args) = self.parse_or_report::<ShellArgs>(call) else {
                        continue;
                    };
                    self.ui.command_start(&args.command);
                    let started = std::time::Instant::now();
                    let (result, output) = self.run_shell_streaming(&args).await?;
                    let duration_ms = started.elapsed().as_millis();
                    self.ui.command_end(result.exit_code, "");
                    if let Some(l) = &mut self.logger {
                        l.log_command(&args.command, result.exit_code, duration_ms, &output);
                    }
                    let truncated = truncate(&output, self.behavior.max_command_output_bytes);
                    let observation = format!("[exit code: {}]\n{}", result.exit_code, truncated);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
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
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_PLAN => {
                    let Some(args) = self.parse_or_report::<PlanArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_plan(args);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_ARTIFACT => {
                    let Some(args) = self.parse_or_report::<ArtifactArgs>(call) else {
                        continue;
                    };
                    self.ui.tool_use(&format!("artifact {}", args.action));
                    let observation = self.run_artifact(&args);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_HANDOFF => {
                    let Some(args) = self.parse_or_report::<HandoffArgs>(call) else {
                        continue;
                    };
                    self.ui.tool_use("handoff");
                    let observation = self.run_handoff(&args);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
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
                    let tool_msg =
                        Message::tool_result(&call.id, format!("marked blocked: {}", args.reason));
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_DECISION => {
                    let Some(args) = self.parse_or_report::<DecisionArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_decision(&args);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_UNBLOCK => {
                    self.ui.blocked(None);
                    self.emit_lifecycle(cowboy_core::lifecycle::LifecycleEvent::Unblocked);
                    let tool_msg = Message::tool_result(&call.id, "unblocked".to_string());
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_PROPOSE_SCOPE_CHANGE => {
                    let Some(args) = self.parse_or_report::<ProposeScopeChangeArgs>(call) else {
                        continue;
                    };
                    let observation = self.run_propose_scope_change(&args);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_PROPOSE_RANCH => {
                    let Some(args) = self.parse_or_report::<ProposeRanchArgs>(call) else {
                        continue;
                    };
                    self.ui.tool_use(&format!(
                        "propose_ranch: {} ({} workstreams)",
                        args.title,
                        args.workstreams.len()
                    ));
                    let observation = self.run_propose_ranch(&args);
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
                    let tool_msg = Message::tool_result(&call.id, answer);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_SUBAGENT => {
                    // Already executed in the concurrent pre-pass.
                    let result = sub_results
                        .get(&call.id)
                        .cloned()
                        .unwrap_or_else(|| "subagent error: no result produced".to_string());
                    let tool_msg = Message::tool_result(&call.id, result);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                other => {
                    self.messages.push(Message::tool_result(
                        &call.id,
                        format!("error: unknown tool {other}"),
                    ));
                }
            }
        }
        Ok(None)
    }

    /// Record a tool error as an observation so the model can self-correct.
    fn tool_error(&mut self, id: &str, name: &str, err: &str) {
        let msg = Message::tool_result(
            id,
            format!("error: invalid arguments for `{name}`: {err}; please correct and retry"),
        );
        if let Some(l) = &mut self.logger {
            l.log_message(&msg);
        }
        self.messages.push(msg);
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
        let (result, output) = match self.runtime.fileop(&payload.to_string()).await {
            Ok(v) => v,
            Err(e) => {
                let msg = Message::tool_result(call_id, format!("error: {e}"));
                if let Some(l) = &mut self.logger {
                    l.log_message(&msg);
                }
                self.messages.push(msg);
                return Ok((-1, String::new()));
            }
        };
        let observation = if result.exit_code == 0 {
            output.clone()
        } else {
            format!("error: {}", output.trim())
        };
        let observation = truncate(&observation, self.behavior.max_command_output_bytes);
        let tool_msg = Message::tool_result(call_id, observation);
        if let Some(l) = &mut self.logger {
            l.log_message(&tool_msg);
        }
        self.messages.push(tool_msg);
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
                Some(chunk) = rx.recv() => self.ui.command_output(&chunk),
                res = &mut fut => {
                    while let Ok(chunk) = rx.try_recv() {
                        self.ui.command_output(&chunk);
                    }
                    return res;
                }
            }
        }
    }

    /// Stop managed processes in the container (called on session end).
    pub async fn shutdown(&self) {
        let _ = self.runtime.stop_all_processes().await;
    }

    /// Idle teardown: stop the agent container to free its RAM. The next command
    /// restarts it (via the runtime's `ensure_running`). Used by the worker when a
    /// detached session sits idle past the configured timeout.
    pub async fn stop_container(&self) {
        self.runtime.stop().await;
    }

    /// The configured idle-container timeout (0 = disabled).
    pub fn idle_container_timeout_seconds(&self) -> u64 {
        self.behavior.idle_container_timeout_seconds
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
            async move {
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
                (id, label, result, status, outcome)
            }
        }))
        .buffer_unordered(max_parallel);

        while let Some((id, label, res, status, outcome)) = stream.next().await {
            self.ui.subagent_done(&label, status == "complete");
            if let Some(o) = outcome {
                cowboy_core::crew::record_outcome(&o);
            }
            results.insert(id, res);
        }
        self.ui.notice("↳ subagent(s) finished");
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
        Ok(SubagentPlan {
            exe,
            root: self.runtime.root().to_path_buf(),
            container_name: self.runtime.container_name().to_string(),
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
        self.ui
            .subagent_started(label, plan.model.as_deref().unwrap_or("<default>"));
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Delta>();
        let fut = self.model.chat(&self.messages, &self.tools, Some(tx));
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
    use crate::net::docker::{ContainerState, ExecResult, MockDockerCli};
    use cowboy_core::config::{Mount, SecurityConfig};
    use cowboy_core::model::{ChatResponse, ToolCall};
    use std::sync::Mutex;

    /// A model that returns a scripted sequence of responses.
    struct ScriptedModel {
        responses: Mutex<std::collections::VecDeque<ChatResponse>>,
    }
    impl ScriptedModel {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
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
        fn ask_user(&mut self, _question: &str, _options: &[String]) -> String {
            "yes".to_string()
        }
        fn notice(&mut self, msg: &str) {
            self.notices.push(msg.to_string());
        }
    }

    fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    fn runtime_with(docker: MockDockerCli) -> AgentRuntime {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut security = SecurityConfig {
            container: cowboy_core::config::ContainerConfig {
                image: "img".into(),
                mounts: vec![Mount {
                    source: ".".into(),
                    target: "/workspace".into(),
                    mode: "rw".into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        security.networks.isolated.enabled = false; // no gateway in unit tests
                                                    // Leak the tempdir so it outlives the runtime for the test.
        let root = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        AgentRuntime::new(Box::new(docker), root, security)
            .expect("runtime (isolation off in tests)")
    }

    #[tokio::test]
    async fn runs_shell_then_final() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
        docker
            .expect_exec_stream()
            .withf(|_n, _w, _u, command, _t, _c, _ch| command.contains("ls"))
            .times(1)
            .returning(|_, _, _, _, _, _, chunks| {
                let _ = chunks.send("file1\nfile2\n".into());
                Ok((ExecResult { exit_code: 0 }, "file1\nfile2\n".into()))
            });

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
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime_with(docker),
            behavior,
            200_000,
            cancel,
            &mut ui,
        );
        let final_msg = agent.run("list the files then finish").await.unwrap();

        assert_eq!(final_msg.as_deref(), Some("done; tests pass"));
        assert_eq!(ui.commands, vec!["ls"]);
        assert_eq!(ui.finals, vec!["done; tests pass"]);
    }

    #[tokio::test]
    async fn stops_when_token_budget_reached_and_reports_cost() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
        docker
            .expect_exec_stream()
            .returning(|_, _, _, _, _, _, chunks| {
                let _ = chunks.send("out\n".into());
                Ok((ExecResult { exit_code: 0 }, "out\n".into()))
            });

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
            runtime_with(docker),
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

    #[tokio::test]
    async fn plan_tool_records_steps_and_normalizes_status() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));

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
            runtime_with(docker),
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
            runtime_with(MockDockerCli::new()),
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
            runtime_with(MockDockerCli::new()),
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
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));

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

        let runtime = runtime_with(docker);
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
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));

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

        let runtime = runtime_with(docker);
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
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));

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

        let runtime = runtime_with(docker);
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
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));

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

        let runtime = runtime_with(docker);
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
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));

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

        let runtime = runtime_with(docker);
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
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
        docker
            .expect_exec_stream()
            .returning(|_, _, _, _, _, _, _| Ok((ExecResult { exit_code: 0 }, "same".into())));
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
            runtime_with(docker),
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

    #[test]
    fn with_history_inserts_after_system_in_order() {
        let history = vec![
            Message::user("earlier task"),
            Message::new(Role::Assistant, "earlier answer"),
        ];
        let mut ui = RecordingUi::default();
        let agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            runtime_with(MockDockerCli::new()),
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
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
        docker
            .expect_exec_stdin()
            .withf(|_n, _w, _u, argv, stdin| {
                argv == ["cowboy", "x-fileop"]
                    && stdin.contains("\"op\":\"edit\"")
                    && stdin.contains("main.rs")
            })
            .times(1)
            .returning(|_, _, _, _, _| {
                Ok((
                    ExecResult { exit_code: 0 },
                    "edited main.rs: 1 replacement\n".into(),
                ))
            });

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
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let final_msg = agent.run("edit then finish").await.unwrap();
        assert_eq!(final_msg.as_deref(), Some("done"));
        // The UI showed the helper's status line for the edit.
        assert_eq!(ui.tool_uses, vec!["edited main.rs: 1 replacement"]);
    }

    #[tokio::test]
    async fn plan_mode_blocks_edits_until_approved() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
        // Deliberately set NO `expect_exec_stdin`: if the edit reached the file
        // op, mockall would panic on the unexpected call — so this asserts the
        // gate actually prevents the mutation, not just discourages it.
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
            runtime_with(docker),
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
    async fn propose_ranch_drafts_a_plan_and_rejects_a_bad_graph() {
        // A valid decomposition is written and confirmed.
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
        let good = r#"{"title":"Billing","goal":"stripe + invoicing","workstreams":[
            {"id":"schema","goal":"tables"},
            {"id":"api","goal":"api","depends_on":["schema"]}]}"#;
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "propose_ranch", good)],
            },
            ChatResponse {
                truncated: false,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"drafted"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("plan it").await.unwrap();
        let drafted = agent
            .messages
            .iter()
            .any(|m| m.content.contains("drafted ranch"));
        assert!(drafted, "a valid decomposition should draft a ranch");

        // A cyclic graph is refused (the agent is told to fix and retry).
        let cyclic = r#"{"title":"X","goal":"y","workstreams":[
            {"id":"a","goal":"a","depends_on":["b"]},
            {"id":"b","goal":"b","depends_on":["a"]}]}"#;
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
        let model = ScriptedModel::new(vec![
            ChatResponse {
                truncated: false,
                reasoning: None,
                content: None,
                tool_calls: vec![tool_call("1", "propose_ranch", cyclic)],
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
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.run("plan it").await.unwrap();
        let rejected = agent.messages.iter().any(|m| m.content.contains("invalid"));
        assert!(rejected, "a cyclic decomposition must be rejected");
    }

    #[tokio::test]
    async fn stops_at_max_iterations() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
        docker
            .expect_exec_stream()
            .returning(|_, _, _, _, _, _, _| Ok((ExecResult { exit_code: 0 }, "ok".into())));
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
            runtime_with(docker),
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
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_container_label()
            .returning(|_, _| Ok(Some(env!("CARGO_PKG_VERSION").to_string())));
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
            runtime_with(docker),
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
        let docker = MockDockerCli::new();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            runtime_with(docker),
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
        let docker = MockDockerCli::new();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            runtime_with(docker),
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
    async fn fit_context_prunes_old_history_keeping_system() {
        let docker = MockDockerCli::new();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            RESPONSE_HEADROOM + 20, // tiny effective budget (~20 tokens)
            CancellationToken::new(),
            &mut ui,
        );
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
        assert!(ui.notices.iter().any(|n| n.contains("pruned")));
    }

    #[tokio::test]
    async fn fit_context_compacts_old_turns_into_a_summary() {
        let docker = MockDockerCli::new();
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
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            RESPONSE_HEADROOM + 60,
            CancellationToken::new(),
            &mut ui,
        );
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
        // no content and no tool call with finish_reason=length. The loop must
        // surface that explicitly (so a foreman reading a subagent's stdout sees
        // the cause) rather than returning an empty/None result.
        let docker = MockDockerCli::new();
        let mut ui = RecordingUi::default();
        let model = ScriptedModel::new(vec![ChatResponse {
            truncated: true,
            reasoning: Some("thinking ".repeat(100)),
            content: None,
            tool_calls: vec![],
        }]);
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("review the diff").await.unwrap();
        let msg = res.expect("truncation should yield a descriptive result, not None");
        assert!(msg.starts_with("[incomplete]"), "got: {msg}");
        assert_eq!(classify_subagent_result(&msg), "error");
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
            runtime_with(MockDockerCli::new()),
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
}
