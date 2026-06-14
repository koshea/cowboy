//! The Cowboy-owned agent loop: model turn -> tool call -> observation ->
//! repeat, until `final`, `ask_user` is answered, or limits are hit. Cowboy
//! owns this lifecycle; no agent framework.

use anyhow::Result;
use cowboy_core::config::AgentBehavior;
use cowboy_core::model::{ChatResponse, Delta, Message, ModelClient, Role, ToolDef};
use tokio_util::sync::CancellationToken;

use super::tools::{
    self, AskUserArgs, EditArgs, FinalArgs, MemoryArgs, ReadArgs, ShellArgs, SubagentArgs,
    WriteArgs,
};
use super::ui::AgentUi;
use crate::net::docker::ExecResult;
use crate::net::runtime::AgentRuntime;
use crate::session::SessionLogger;

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
`cowboy skill show <name>` to read a skill's instructions, then follow them. \
For a large, independent sub-task, use the `subagent` tool to delegate it.

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

Before large edits, inspect the repository and form a brief plan. After edits, run \
relevant checks. When finished, call `final` summarizing what changed, what was \
validated, and remaining risks or follow-up work.";

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
    messages: Vec<Message>,
    ui: &'a mut dyn AgentUi,
    logger: Option<SessionLogger>,
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
        Self {
            model,
            runtime,
            tools: tools::definitions(),
            behavior,
            cancel,
            context_window,
            pruned_notified: false,
            subagent_depth: std::env::var("COWBOY_SUBAGENT_DEPTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            last_final: None,
            tokens_in: 0,
            tokens_out: 0,
            price_in: None,
            price_out: None,
            cost_usd: 0.0,
            budget_warned: false,
            messages: vec![Message::system(SYSTEM_PROMPT)],
            ui,
            logger: None,
        }
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
        let budget = self.context_window.saturating_sub(RESPONSE_HEADROOM);
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

    /// Run one conversational turn for `task`, keeping the conversation (and the
    /// session logger) alive for subsequent turns. `turn_cancel` interrupts just
    /// this turn. Does NOT finalize the session.
    pub async fn run_turn(
        &mut self,
        task: &str,
        turn_cancel: CancellationToken,
    ) -> Result<Option<String>> {
        self.cancel = turn_cancel;
        let outcome = self.run_inner(task).await;
        if let Ok(Some(m)) = &outcome {
            self.last_final = Some(m.clone());
        }
        outcome
    }

    /// Finalize the session log (diff + summary). Call once when the
    /// conversation ends.
    pub fn finalize_session(&self) {
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
        self.finalize_session();
        outcome
    }

    /// Run the loop for `task` until completion, cancellation, or the iteration
    /// cap. Returns the final message if the agent produced one.
    async fn run_inner(&mut self, task: &str) -> Result<Option<String>> {
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

            // Record the assistant turn (content + any tool calls).
            let assistant = Message {
                role: Role::Assistant,
                content: response.content.clone().unwrap_or_default(),
                tool_call_id: None,
                tool_calls: response.tool_calls.clone(),
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
                self.ui.notice("model produced no action; stopping");
                return Ok(None);
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

    /// Process this turn's tool calls. Returns `Some(message)` if `final` was
    /// called.
    async fn handle_tool_calls(&mut self, response: &ChatResponse) -> Result<Option<String>> {
        for call in &response.tool_calls {
            match call.name.as_str() {
                tools::TOOL_FINAL => {
                    let args: FinalArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    if let Some(l) = &self.logger {
                        l.write_final(&args.message);
                    }
                    self.ui.final_message(&args.message);
                    return Ok(Some(args.message));
                }
                tools::TOOL_SHELL => {
                    let args: ShellArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
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
                    let args: ReadArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    self.ui.tool_use(&format!("read {}", args.path));
                    let payload = serde_json::json!({
                        "op": "read", "path": args.path,
                        "offset": args.offset, "limit": args.limit,
                    });
                    self.run_fileop(&call.id, &payload).await?;
                }
                tools::TOOL_EDIT => {
                    let args: EditArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    let payload = serde_json::json!({
                        "op": "edit", "path": args.path,
                        "old": args.old, "new": args.new, "replace_all": args.replace_all,
                    });
                    let (exit, out) = self.run_fileop(&call.id, &payload).await?;
                    self.ui
                        .tool_use(&fileop_summary("edit", &args.path, exit, &out));
                }
                tools::TOOL_WRITE => {
                    let args: WriteArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    let payload = serde_json::json!({
                        "op": "write", "path": args.path, "content": args.content,
                    });
                    let (exit, out) = self.run_fileop(&call.id, &payload).await?;
                    self.ui
                        .tool_use(&fileop_summary("write", &args.path, exit, &out));
                }
                tools::TOOL_MEMORY => {
                    let args: MemoryArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    self.ui.tool_use(&format!("memory {}", args.action));
                    let observation = self.run_memory(&args);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_ASK_USER => {
                    let args: AskUserArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    let answer = self.ui.ask_user(&args.question);
                    let tool_msg = Message::tool_result(&call.id, answer);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_SUBAGENT => {
                    let args: SubagentArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    let result = self.run_subagent(&args).await;
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

    /// Handle a `memory` tool call host-side (the agent can't reach the home
    /// dir; the loop runs on the host, so it reads/writes it directly). Returns
    /// the observation text.
    fn run_memory(&self, args: &MemoryArgs) -> String {
        use cowboy_core::memory::{self, Scope};
        let key = format!(
            "{:08x}",
            crate::net::runtime::project_hash(self.runtime.root())
        );
        match args.action.as_str() {
            "save" => {
                let (Some(title), Some(content)) = (&args.title, &args.content) else {
                    return "error: `save` requires `title` and `content`".into();
                };
                let scope = match args.scope.as_deref() {
                    Some("global") => Scope::Global,
                    _ => Scope::Project,
                };
                match memory::save(&key, title, content, scope, args.kind.as_deref()) {
                    Ok(name) => format!("saved memory `{name}` [{}]", scope.as_str()),
                    Err(e) => format!("error: {e}"),
                }
            }
            "recall" => {
                let Some(name) = &args.name else {
                    return "error: `recall` requires `name`".into();
                };
                match memory::recall(&key, name) {
                    Ok(Some(body)) => body,
                    Ok(None) => format!("no memory named `{name}`"),
                    Err(e) => format!("error: {e}"),
                }
            }
            "list" => {
                let idx = memory::index(&key);
                if idx.is_empty() {
                    "no memories stored".into()
                } else {
                    idx
                }
            }
            "delete" => {
                let Some(name) = &args.name else {
                    return "error: `delete` requires `name`".into();
                };
                match memory::delete(&key, name) {
                    Ok(true) => format!("deleted memory `{name}`"),
                    Ok(false) => format!("no memory named `{name}`"),
                    Err(e) => format!("error: {e}"),
                }
            }
            other => {
                format!("error: unknown memory action `{other}` (save|recall|list|delete)")
            }
        }
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

    /// Run a structured file operation in the container, record the observation
    /// for the model, and log it. Returns (exit_code, helper output).
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

    /// Run a subagent by recursively invoking the `cowboy` CLI in one-shot mode,
    /// reusing this session's container (via `COWBOY_CONTAINER_NAME`) so the
    /// subagent shares the workspace and gateway. Returns its final answer.
    async fn run_subagent(&mut self, args: &SubagentArgs) -> String {
        if self.subagent_depth >= MAX_SUBAGENT_DEPTH {
            return format!(
                "error: maximum subagent depth ({MAX_SUBAGENT_DEPTH}) reached; do this work directly"
            );
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return format!("subagent error: cannot locate cowboy binary: {e}"),
        };
        let task = match &args.context {
            Some(ctx) if !ctx.is_empty() => format!("{ctx}\n\n{}", args.task),
            _ => args.task.clone(),
        };
        self.ui.notice(&format!("↳ subagent: {}", args.task));

        let output = tokio::process::Command::new(exe)
            .arg(&task)
            .current_dir(self.runtime.root())
            .env("COWBOY_CONTAINER_NAME", self.runtime.container_name())
            .env(
                "COWBOY_SUBAGENT_DEPTH",
                (self.subagent_depth + 1).to_string(),
            )
            .env("COWBOY_PRINT_FINAL_ONLY", "1")
            // Don't let the child's logs corrupt the parent TUI/console.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;

        match output {
            Ok(o) => {
                self.ui.notice("↳ subagent finished");
                let result = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if result.is_empty() {
                    "subagent produced no final answer".to_string()
                } else {
                    result
                }
            }
            Err(e) => format!("subagent failed to start: {e}"),
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
/// future holds an immutable borrow of the loop).
fn emit_delta(ui: &mut dyn AgentUi, piece: Delta) {
    match piece {
        Delta::Content(t) => ui.model_delta(&t),
        Delta::Reasoning(t) => ui.model_reasoning(&t),
    }
}

/// Render a span of messages as plain text for the compaction summarizer.
fn render_transcript(messages: &[Message]) -> String {
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

fn parse_args<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T> {
    let args = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str(args).map_err(|e| anyhow::anyhow!("invalid tool arguments: {e}"))
}

/// A concise one-line summary of a file op for the UI: the helper's status line
/// on success, or `"<action> <path> — failed"` otherwise.
fn fileop_summary(action: &str, path: &str, exit: i32, output: &str) -> String {
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

/// Truncate `output` to at most `max_bytes`, on a char boundary, with a marker.
fn truncate(output: &str, max_bytes: usize) -> String {
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
    }
    impl AgentUi for RecordingUi {
        fn model_delta(&mut self, _text: &str) {}
        fn cost(&mut self, usd: f64) {
            self.costs.push(usd);
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
        fn ask_user(&mut self, _question: &str) -> String {
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
    }

    #[tokio::test]
    async fn runs_shell_then_final() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
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
                content: Some("inspecting".into()),
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"ls"}"#)],
            },
            ChatResponse {
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
            .expect_exec_stream()
            .returning(|_, _, _, _, _, _, chunks| {
                let _ = chunks.send("out\n".into());
                Ok((ExecResult { exit_code: 0 }, "out\n".into()))
            });

        // The model keeps asking for shell (never finals); only the budget stops it.
        let model = ScriptedModel::new(vec![
            ChatResponse {
                content: Some("working".into()),
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"ls"}"#)],
            },
            ChatResponse {
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
    async fn runs_edit_via_fileop_then_final() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
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
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "edit",
                    r#"{"path":"main.rs","old":"foo","new":"bar"}"#,
                )],
            },
            ChatResponse {
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
    async fn stops_at_max_iterations() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
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
        let model = ScriptedModel::new(vec![
            ChatResponse {
                content: None,
                tool_calls: vec![tool_call("1", "final", r#"{"message":"done 1"}"#)],
            },
            ChatResponse {
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
        let result = agent
            .run_subagent(&super::super::tools::SubagentArgs {
                task: "do a thing".into(),
                context: None,
            })
            .await;
        // At max depth it returns an error string without spawning a subprocess.
        assert!(result.contains("maximum subagent depth"), "got: {result}");
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
}
