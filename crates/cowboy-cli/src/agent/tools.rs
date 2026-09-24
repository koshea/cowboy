//! The agent's tool surface: `shell` for commands, structured `read`/`edit`/
//! `write` for files, plus `final`, `ask_user`, and `subagent`. Cowboy-specific
//! capabilities (patch, proc, skill) remain CLIs the agent calls *through*
//! `shell`, never built-in tools.

use cowboy_core::model::ToolDef;
use schemars::JsonSchema;
use serde::Deserialize;

pub const TOOL_SHELL: &str = "shell";
pub const TOOL_FINAL: &str = "final";
pub const TOOL_ASK_USER: &str = "ask_user";
pub const TOOL_SUBAGENT: &str = "subagent";
pub const TOOL_JOBS: &str = "jobs";
pub const TOOL_WAIT: &str = "wait";
pub const TOOL_JOB_REPLY: &str = "job_reply";
pub const TOOL_REQUEST_TURNS: &str = "request_turns";
pub const TOOL_READ: &str = "read";
pub const TOOL_EDIT: &str = "edit";
pub const TOOL_WRITE: &str = "write";
pub const TOOL_GREP: &str = "grep";
pub const TOOL_LS: &str = "ls";
pub const TOOL_PROC: &str = "proc";
pub const TOOL_MEMORY: &str = "memory";
pub const TOOL_PLAN: &str = "plan";
pub const TOOL_ARTIFACT: &str = "artifact";
pub const TOOL_HANDOFF: &str = "handoff";
pub const TOOL_BLOCKED: &str = "blocked";
pub const TOOL_UNBLOCK: &str = "unblock";
pub const TOOL_DECISION: &str = "decision";
pub const TOOL_REQUEST_PATH: &str = "request_path";
pub const TOOL_PROPOSE_SCOPE_CHANGE: &str = "propose_scope_change";
/// Conditional: added only when ≥1 MCP server is enabled (see [`mcp_definition`]).
pub const TOOL_MCP: &str = "mcp";

/// The only tools a worker may call once it has been told to wrap up.
///
/// Wrapping up means the turn budget is spent and the worker has just enough left to
/// *report*. Before this list existed the directive was advisory, and a worker that
/// ignored it kept full access: one real subagent spent its entire wrap-up extension
/// on fourteen more `grep`s, was stopped, and lost seventy turns of investigation
/// because it had written nothing down. Telling a model to stop investigating is not
/// the same as stopping it.
///
/// Reporting, recording and *collecting* are allowed; investigating and mutating are
/// not. `jobs`/`wait`/`job_reply` are in the list deliberately: a foreman may enter
/// wrap-up with subagents still in flight, and `final` refuses while they are — so
/// denying it the means to collect them would deadlock the very path this protects.
pub const WRAP_UP_ALLOWED: &[&str] = &[
    // The report itself, and the durable forms of it.
    TOOL_FINAL,
    TOOL_ARTIFACT,
    TOOL_HANDOFF,
    TOOL_DECISION,
    TOOL_MEMORY,
    // Reporting that the work could not be finished is a legitimate outcome.
    TOOL_BLOCKED,
    TOOL_UNBLOCK,
    // Collecting delegated work so there is something to report.
    TOOL_JOBS,
    TOOL_WAIT,
    TOOL_JOB_REPLY,
];

/// Whether `name` may still be called while wrapping up. See [`WRAP_UP_ALLOWED`].
pub fn allowed_when_wrapping_up(name: &str) -> bool {
    WRAP_UP_ALLOWED.contains(&name)
}

/// Arguments for the `shell` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// The shell command to run in the sandbox, executed with `/bin/sh -c` — which
    /// is dash on Debian/Ubuntu, so keep it POSIX (no `[[`, no `<(…)`, no
    /// `exec -a`). **Every call is a fresh shell in a fresh process**, so a `cd` or
    /// an `export` does not carry over to the next call; the filesystem and any
    /// server left listening do. To run somewhere else, pass `cwd` or chain it:
    /// `cd sub && cargo test`.
    pub command: String,
    /// Optional working directory for this command (defaults to the workspace root).
    /// This is how to run in a subdirectory, since `cd` does not persist.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional timeout in seconds for this command. Raise it for a long test or
    /// build suite; lower it for a command you expect to be quick so a hang is cut
    /// short. Omit to use the session default. The host clamps it to a ceiling.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// Arguments for the `final` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FinalArgs {
    /// A summary of what changed, what was validated, and any follow-ups.
    pub message: String,
}

/// Arguments for the `ask_user` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AskUserArgs {
    /// The question. State the decision and the context needed to make it; keep it
    /// to a few sentences — detail belongs in each option's `description`.
    pub question: String,
    /// The answers to choose between (2–4). The user can always type their own
    /// answer instead, so do not add an "other" option.
    #[serde(default)]
    pub options: Option<Vec<AskOptionArg>>,
}

/// One option for `ask_user`: a short label, optionally with what it means and
/// whether you recommend it. A bare string is accepted as a label.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AskOptionArg {
    Label(String),
    Full {
        /// A few words (≤ ~8) — what the user picks and what you get back.
        label: String,
        /// One or two sentences on what this choice means or implies.
        #[serde(default)]
        description: Option<String>,
        /// Mark the single option you recommend.
        #[serde(default)]
        recommended: bool,
    },
}

impl AskOptionArg {
    fn into_choice(self) -> cowboy_core::daemonproto::AskChoice {
        match self {
            AskOptionArg::Label(label) => label.into(),
            AskOptionArg::Full {
                label,
                description,
                recommended,
            } => cowboy_core::daemonproto::AskChoice {
                label,
                description: description.filter(|d| !d.trim().is_empty()),
                recommended,
            },
        }
    }
}

impl AskUserArgs {
    /// The options as wire choices, with at most one recommendation (the first
    /// marked) so the pre-selection is unambiguous.
    pub fn choices(&self) -> Vec<cowboy_core::daemonproto::AskChoice> {
        let mut seen = false;
        self.options
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(AskOptionArg::into_choice)
            .filter(|c| !c.label.trim().is_empty())
            .map(|mut c| {
                if c.recommended {
                    c.recommended = !seen;
                    seen = true;
                }
                c
            })
            .collect()
    }
}

/// Arguments for the `subagent` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SubagentArgs {
    /// A focused, self-contained task for the subagent to complete.
    pub task: String,
    /// Optional extra context to prepend (the subagent starts with a fresh
    /// conversation, so include anything it needs to know).
    #[serde(default)]
    pub context: Option<String>,
    /// The KIND of work, so Cowboy routes it to the right crew model. Use one of
    /// the categories listed in your system prompt (they come from the user's
    /// roster — typically general, exploration, backend, frontend, tests, docs,
    /// debugging, refactor, e2e, review). Name the work by the artifact it
    /// produces, not the subject it touches. An unlisted category silently falls
    /// back to `general`. Defaults to `general`. Do NOT name a model — routing is
    /// the user's crew roster.
    #[serde(default)]
    pub category: Option<String>,
    /// How hard the task is: tiny, small, medium, large, or deep. Defaults to
    /// `medium`. Sets BOTH the model and the worker's turn grant, so it is the
    /// cost dial — judge difficulty only, never urgency or importance. Your
    /// system prompt gives the turn count each level buys on this roster. When
    /// torn between two levels, pick the lower: a worker can ask for more turns,
    /// but an over-sized effort overpays on every token.
    #[serde(default)]
    pub effort: Option<String>,
    /// Why you're delegating this (one line) — recorded with the routing decision.
    #[serde(default)]
    pub reason: Option<String>,
    /// The concrete artifact you expect back (e.g. "changed test files + summary").
    #[serde(default)]
    pub expected_artifact: Option<String>,
    /// Optional: a named agent definition to adopt (from `.claude/agents/` or
    /// `.cowboy/agents/`, e.g. "security-reviewer"). Its instructions are
    /// prepended so the worker takes on that persona. Discover names with
    /// `cowboy agents list`. (The crew still picks the model from category/effort.)
    #[serde(default)]
    pub agent: Option<String>,
    /// Run this on an external agent harness (one listed under "Harnesses" in your
    /// system prompt, e.g. "grok") instead of a crew model. ONLY when the user
    /// explicitly asks for that harness — otherwise leave it out and let the roster
    /// route. The harness works in the same workspace and reports back like any
    /// subagent.
    #[serde(default)]
    pub harness: Option<String>,
}

/// Arguments for the `jobs` tool — no arguments; it lists everything.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct JobsArgs {}

/// Arguments for the `wait` tool.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct WaitArgs {
    /// Job ids to wait for. Omit to wait for whichever background job reports next.
    #[serde(default)]
    pub ids: Option<Vec<String>>,
    /// Wait for *all* the named jobs (or all running jobs) instead of returning as
    /// soon as the first one reports. Defaults to false.
    #[serde(default)]
    pub all: Option<bool>,
    /// Give up waiting after this many seconds and return so you can do something
    /// else. Defaults to a few minutes; capped by the host.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// Arguments for the `job_reply` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct JobReplyArgs {
    /// The job asking for more turns, or asking a question.
    pub id: String,
    /// One of: `answer` (reply to a question), `grant` (more turns), `redirect` (more
    /// turns, different approach), `wrap_up` (stop exploring and write up what it has),
    /// `stop` (abandon it).
    pub verdict: String,
    /// How many more turns to grant, for `grant`/`redirect`. The host clamps this to
    /// the worker's remaining ceiling.
    #[serde(default)]
    pub iterations: Option<u32>,
    /// What to do differently — required for `redirect`, the reason for `stop`, and the
    /// reply itself for `answer`.
    #[serde(default)]
    pub instructions: Option<String>,
}

/// Arguments for the `request_turns` tool (a delegated worker asking its foreman for
/// more turns).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RequestTurnsArgs {
    /// What you have established so far — concrete findings, not "making progress".
    pub progress: String,
    /// What is still left to do.
    pub remaining: String,
    /// The single next concrete step you would take.
    pub next_step: String,
    /// How many more turns you need to finish.
    pub iterations: u32,
}

/// Arguments for the `read` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// Path to the file (workspace-relative, e.g. `src/main.rs`).
    pub path: String,
    /// 1-based line to start at (default 1).
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to return.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One find/replace within a single file, as an entry in `edits`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditSpecArgs {
    /// Exact text to replace. Must match a unique span unless `replace_all`.
    pub old: String,
    /// Replacement text.
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

/// Arguments for the `edit` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditArgs {
    /// Path to the file to edit (workspace-relative).
    pub path: String,
    /// Exact text to replace. Must match a unique span unless `replace_all`.
    /// Omit when using `edits`.
    #[serde(default)]
    pub old: Option<String>,
    /// Replacement text. Omit when using `edits`.
    #[serde(default)]
    pub new: Option<String>,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
    /// Several edits to the same file, applied in order and **all-or-nothing**:
    /// if any one fails, the file is left untouched. Use instead of `old`/`new`
    /// when changing a file in more than one place.
    #[serde(default)]
    pub edits: Vec<EditSpecArgs>,
}

/// Arguments for the `write` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// Path to write (workspace-relative). Parent directories are created.
    pub path: String,
    /// Full file contents (overwrites any existing file).
    pub content: String,
}

/// Arguments for the `grep` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Regular expression to search for (Rust regex syntax).
    pub pattern: String,
    /// Limit the search to this file or directory (workspace-relative).
    #[serde(default)]
    pub path: Option<String>,
    /// Only search files whose name or workspace-relative path matches this glob,
    /// e.g. `*.rs` or `src/**/mod.rs`.
    #[serde(default)]
    pub glob: Option<String>,
    /// Treat `pattern` as plain text rather than a regex.
    #[serde(default)]
    pub literal: bool,
    /// Case-insensitive matching.
    #[serde(default)]
    pub case_insensitive: bool,
    /// Maximum matches to report (default 200). Totals are reported regardless.
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Lines of surrounding context to show on each side of a match (like
    /// `grep -C`). Match lines use `path:line:`, context lines `path:line-`.
    #[serde(default)]
    pub context: Option<usize>,
    /// Report only the paths of files that contain a match, not the lines (like
    /// `grep -l`). Cheaper when you only need to know *where* something is.
    #[serde(default)]
    pub files_only: bool,
    /// Also search files and directories `.gitignore` excludes (build output,
    /// generated code, vendored dependencies). Off by default; the result says when
    /// something was hidden.
    #[serde(default)]
    pub include_ignored: bool,
}

/// Arguments for the `ls` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct LsArgs {
    /// Directory to list (workspace-relative). Defaults to the workspace root.
    #[serde(default)]
    pub path: Option<String>,
    /// Only list files whose name or workspace-relative path matches this glob,
    /// e.g. `*.rs` or `src/**/mod.rs`.
    #[serde(default)]
    pub glob: Option<String>,
    /// Walk the whole tree beneath `path` instead of just one level. Build and
    /// dependency directories (`target`, `node_modules`, …) are always skipped.
    #[serde(default)]
    pub recursive: bool,
    /// Maximum entries to report (default 500). The true total is reported regardless.
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Also list files and directories `.gitignore` excludes. Off by default; the
    /// result says when something was hidden.
    #[serde(default)]
    pub include_ignored: bool,
}

/// Arguments for the `proc` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ProcArgs {
    /// "start", "stop", "restart", "list" or "logs".
    pub action: String,
    /// The process name — required for everything but `list`. Either a name from
    /// `.cowboy/agent.yaml`'s `processes:` (whose command is already defined) or one
    /// you choose for an ad-hoc process, in which case pass `command` too.
    #[serde(default)]
    pub name: Option<String>,
    /// For `start`/`restart` of an ad-hoc process: the command to run.
    #[serde(default)]
    pub command: Option<String>,
    /// For `start`/`restart`: the working directory (defaults to the workspace root).
    #[serde(default)]
    pub cwd: Option<String>,
    /// For `logs`: how many trailing lines to show (default 80).
    #[serde(default)]
    pub lines: Option<usize>,
}

/// Arguments for the `memory` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MemoryArgs {
    /// What to do: "save", "recall", "list", or "delete".
    pub action: String,
    /// For `save`: a one-line title (becomes the index description and slug).
    #[serde(default)]
    pub title: Option<String>,
    /// For `save`: the full memory body to store.
    #[serde(default)]
    pub content: Option<String>,
    /// For `recall`/`delete`: the memory name (slug) shown in the index.
    #[serde(default)]
    pub name: Option<String>,
    /// For `save`: "project" (default) or "global".
    #[serde(default)]
    pub scope: Option<String>,
    /// For `save`: a free-form category, e.g. "preference" or "fact".
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// One step in the agent's working plan.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlanStep {
    /// A short description of the step.
    pub step: String,
    /// Status: "pending" (default), "in_progress", or "done".
    #[serde(default)]
    pub status: Option<String>,
}

/// Arguments for the `plan` tool. The whole list is replaced on each call.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlanArgs {
    /// The full, ordered list of steps; replaces the current plan.
    pub steps: Vec<PlanStep>,
}

/// Arguments for the `handoff` tool — a structured end-of-session summary that
/// the next worker (or a Ranch coordinator) can rely on.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HandoffArgs {
    /// What this session set out to do.
    pub goal: String,
    /// Outcome: complete | partial | blocked | failed.
    pub status: String,
    /// Files created/changed (and how), if any.
    #[serde(default)]
    pub changed_files: Option<String>,
    /// Important decisions made and why.
    #[serde(default)]
    pub decisions: Option<String>,
    /// Interfaces/contracts introduced or changed (point at published artifacts).
    #[serde(default)]
    pub contracts: Option<String>,
    /// How the work was validated (tests/builds run and their result).
    #[serde(default)]
    pub validation: Option<String>,
    /// Known risks or gaps.
    #[serde(default)]
    pub risks: Option<String>,
    /// Recommended next steps for whoever picks this up.
    #[serde(default)]
    pub next_steps: Option<String>,
}

/// Arguments for the `decision` tool — ask the user to decide and record it.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DecisionArgs {
    /// The decision to make (asked to the user).
    pub question: String,
    /// Optional choices to present.
    #[serde(default)]
    pub options: Option<Vec<String>>,
    /// Optional rationale/context to record alongside the decision.
    #[serde(default)]
    pub rationale: Option<String>,
}

/// Arguments for the `blocked` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BlockedArgs {
    /// Why you cannot proceed (e.g. "need the API contract from the schema work").
    pub reason: String,
    /// Optional: what you're waiting on (artifact names, workstream ids, a person).
    #[serde(default)]
    pub waiting_on: Option<Vec<String>>,
}

/// Arguments for the `request_path` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RequestPathArgs {
    /// The host path you need, e.g. `/home/me/other-project` or `~/datasets`.
    pub path: String,
    /// Why you need it, in one line. Shown to the user verbatim — this is what they
    /// decide on, so be specific ("the shared proto definitions the client imports",
    /// not "for the task").
    pub reason: String,
    /// Whether read-only access is enough. Ask for read-only unless you must write:
    /// it is far more likely to be approved. Defaults to read-only.
    #[serde(default)]
    pub read_only: Option<bool>,
}

/// Arguments for the `propose_scope_change` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ProposeScopeChangeArgs {
    /// One-line summary of the proposed change.
    pub summary: String,
    /// Why the plan should change (what you learned that the plan didn't anticipate).
    #[serde(default)]
    pub rationale: Option<String>,
    /// The change: "add_workstream", "remove_workstream", or "note" (a concern with
    /// no concrete edit).
    pub change: String,
    /// For add/remove: the workstream id.
    #[serde(default)]
    pub workstream_id: Option<String>,
    /// For add_workstream: a short title.
    #[serde(default)]
    pub title: Option<String>,
    /// For add_workstream: the goal/description.
    #[serde(default)]
    pub goal: Option<String>,
    /// For add_workstream: ids it depends on.
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
}

/// Arguments for the `artifact` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ArtifactArgs {
    /// "publish" (store an output) or "list" (show this session's artifacts).
    pub action: String,
    /// For `publish`: a workspace-relative file to publish (e.g. `docs/api.md`).
    #[serde(default)]
    pub path: Option<String>,
    /// For `publish`: inline content instead of (or in addition to) `path`.
    #[serde(default)]
    pub content: Option<String>,
    /// For `publish`: kind — contract|summary|patch|diff|test_result|notes|review|other.
    #[serde(default)]
    pub kind: Option<String>,
    /// For `publish`: a short human title (defaults to the file name).
    #[serde(default)]
    pub title: Option<String>,
    /// For `publish`: a one-line summary of what the artifact is/contains.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Arguments for the `mcp` tool (present only when MCP servers are connected).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct McpArgs {
    /// "list_tools" to discover a server's tools, or "call" to invoke one.
    pub action: String,
    /// The MCP server name (from the connected list in your context). For
    /// `list_tools`, omit to get a compact listing across all servers, or pass a
    /// name for that server's full tool schemas. Required for `call`.
    #[serde(default)]
    pub server: Option<String>,
    /// For `call`: the tool name to invoke (as shown by `list_tools`).
    #[serde(default)]
    pub tool: Option<String>,
    /// For `call`: the tool's arguments as a JSON object matching its input schema.
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
}

fn schema_for<T: JsonSchema>() -> serde_json::Value {
    // Inline subschemas rather than emitting `$defs` + `$ref`. Some models served
    // through OpenAI-compatible gateways (observed: minimax-m3 on Fireworks) don't
    // resolve `$ref`, so a nested array-of-objects param — e.g. `plan`'s `steps`
    // (Vec<PlanStep>) — never reaches the model with its `{step, status}` shape and
    // it emits empty placeholders (`{"steps": ["", "", ...]}`). Inlining hands every
    // model the concrete shape directly. None of these arg types are recursive, so
    // inlining can't blow up.
    let settings =
        schemars::generate::SchemaSettings::default().with(|s| s.inline_subschemas = true);
    let schema = settings.into_generator().into_root_schema_for::<T>();
    serde_json::to_value(schema).unwrap_or_else(|_| serde_json::json!({}))
}

/// The `mcp` tool definition. Kept out of [`definitions`] and added to the surface
/// by the agent loop only when ≥1 MCP server is enabled, so sessions without MCP
/// don't carry it. The connected servers themselves are named in the system prompt.
pub fn mcp_definition() -> ToolDef {
    ToolDef {
        name: TOOL_MCP.into(),
        description: "Use a connected MCP server's tools. The servers available to you (and what \
                      each is for) are listed in your context. Two actions: `list_tools` to \
                      discover a server's tools — pass `server` for that server's full tool \
                      schemas, or omit `server` for a compact listing across all servers — and \
                      `call` to invoke a tool (`server` + `tool` + `arguments` matching its input \
                      schema). Discover a server's tools with `list_tools` before calling them."
            .into(),
        parameters: schema_for::<McpArgs>(),
    }
}

/// The tool definitions advertised to the model.
pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: TOOL_SHELL.into(),
            description: "Run a shell command inside the sandbox and observe its output. \
                          Use this for builds, tests, git, and cowboy CLIs like `cowboy patch`. \
                          Each call is a FRESH shell: `cd` and `export` do not persist to the \
                          next call — pass `cwd`, or chain with `&&`. The filesystem and any \
                          listening server do persist. For reading, searching or editing files, \
                          prefer the `read`/`grep`/`ls`/`edit`/`write` tools."
                .into(),
            parameters: schema_for::<ShellArgs>(),
        },
        ToolDef {
            name: TOOL_READ.into(),
            description: "Read a file from the workspace with line numbers. Prefer this over \
                          `cat`/`sed` — the line numbers help you make precise edits."
                .into(),
            parameters: schema_for::<ReadArgs>(),
        },
        ToolDef {
            name: TOOL_EDIT.into(),
            description: "Replace an exact span of text in a file. `old` must match exactly and \
                          be unique unless `replace_all` is set. To change a file in several \
                          places, pass `edits` — they apply in order and all-or-nothing, so a \
                          failure leaves the file untouched. Copy `old` from `read` output \
                          without the line-number gutter. Prefer this over `sed`/heredocs — it \
                          is precise and fails loudly if `old` is missing or ambiguous."
                .into(),
            parameters: schema_for::<EditArgs>(),
        },
        ToolDef {
            name: TOOL_GREP.into(),
            description: "Search the workspace for a regex, reporting `path:line:text`. Use this \
                          to find where something lives instead of `grep -r`/`rg` via `shell`: it \
                          skips build output, dependency directories (`target`, `node_modules`, \
                          `.git`, …), anything `.gitignore` excludes, and binary files; it caps \
                          the matches it prints, and always reports the true total so you can \
                          tell when to narrow the pattern. Set `context` for surrounding lines \
                          (like `grep -C`), `files_only` to list just the matching file paths, or \
                          `include_ignored` to search generated output too. Available in plan mode."
                .into(),
            parameters: schema_for::<GrepArgs>(),
        },
        ToolDef {
            name: TOOL_LS.into(),
            description: "List the entries of a workspace directory, one workspace-relative path \
                          per line (directories suffixed with `/`). Use this instead of `ls`/\
                          `find` via `shell`: it skips build output, dependency directories \
                          (`target`, `node_modules`, …) and anything `.gitignore` excludes, caps \
                          the listing, and reports the true total. Pass `recursive` to walk the \
                          whole tree, `glob` to filter files, `path` to list a subdirectory, or \
                          `include_ignored` to include generated output. Available in plan mode."
                .into(),
            parameters: schema_for::<LsArgs>(),
        },
        ToolDef {
            name: TOOL_WRITE.into(),
            description: "Create a new file or overwrite an existing one with the given content. \
                          Parent directories are created. Prefer this over `echo >`/heredocs. \
                          Overwriting a file you have not `read` in this session is refused, as \
                          is one that changed on disk since you read it — `read` it first, or use \
                          `edit` to change only the part you mean to."
                .into(),
            parameters: schema_for::<WriteArgs>(),
        },
        ToolDef {
            name: TOOL_PROC.into(),
            description: "Run a long-lived process in the background — a dev server, an API you \
                          need to send requests to, a watcher. Use this instead of `shell` for \
                          anything that does not exit on its own: a `shell` command gets its own \
                          sandbox whose whole process tree is reaped when it returns, so `&` or \
                          `nohup` there leaves you nothing, and running it in the foreground just \
                          burns the timeout. `start` (with `name`, plus `command` unless the name \
                          is defined in agent.yaml's `processes:`), then keep working — it stays \
                          reachable on localhost from your later `shell` commands. `logs` shows \
                          its recent output (it is also a file: `.cowboy/proc/<name>.log`), \
                          `list` shows what is running, `stop`/`restart` control it. Processes \
                          end with the session."
                .into(),
            parameters: schema_for::<ProcArgs>(),
        },
        ToolDef {
            name: TOOL_MEMORY.into(),
            description: "Your durable cross-session memory (stored on the host, not in the \
                          repo). `save` a concise fact or user preference worth remembering next \
                          time (scope \"project\" by default, or \"global\" across projects); \
                          `recall` a full entry by name; `list` the index; `delete` one. The \
                          index of saved memories is shown to you at the start of each session. \
                          Do NOT use this for project conventions that belong in the repo — put \
                          those in AGENTS.md."
                .into(),
            parameters: schema_for::<MemoryArgs>(),
        },
        ToolDef {
            name: TOOL_PLAN.into(),
            description: "Maintain a short, visible checklist for a multi-step task. Pass the \
                          full ordered list of `steps` (each with a `status` of \"pending\", \
                          \"in_progress\", or \"done\"); the list REPLACES the previous plan. \
                          Create a plan before starting non-trivial work, mark exactly one step \
                          \"in_progress\" at a time, and update statuses as you go. Skip it for \
                          trivial one-step tasks."
                .into(),
            parameters: schema_for::<PlanArgs>(),
        },
        ToolDef {
            name: TOOL_ARTIFACT.into(),
            description: "Publish a durable, typed output others (or a later session) can \
                          consume — a contract, summary, test result, patch, notes, etc. \
                          `publish` with a workspace `path` or inline `content`, a `kind`, a \
                          `title`, and a one-line `summary`; `list` shows this session's \
                          artifacts. Prefer publishing a concrete artifact (e.g. an API/schema \
                          contract) over describing it only in prose."
                .into(),
            parameters: schema_for::<ArtifactArgs>(),
        },
        ToolDef {
            name: TOOL_HANDOFF.into(),
            description: "Write a structured handoff summary for whoever continues this work \
                          (a teammate, a later session, or a Ranch coordinator): goal, status, \
                          changed files, decisions, contracts, validation, risks, next steps. \
                          Call this at the end of a substantial task, just before `final`."
                .into(),
            parameters: schema_for::<HandoffArgs>(),
        },
        ToolDef {
            name: TOOL_DECISION.into(),
            description: "Ask the user to make a decision and record it durably (question, \
                          options, chosen answer, rationale) so the rationale survives and \
                          downstream work can depend on it. Use for choices that shape the work \
                          (data model, protocol, API shape), not routine questions."
                .into(),
            parameters: schema_for::<DecisionArgs>(),
        },
        ToolDef {
            name: TOOL_REQUEST_PATH.into(),
            description: "Ask the user for access to a host path outside the workspace. Use this \
                          when a command failed because a file or directory does not exist or \
                          cannot be read, and you believe the path is legitimately needed \
                          (a sibling repository, a dataset, a shared toolchain). The user \
                          approves or denies; on approval the path is available to the NEXT \
                          command, so re-run the command that failed. Already-running processes \
                          keep their old view and must be restarted. Credential stores \
                          (~/.aws, ~/.ssh, ~/.gnupg, browser profiles, …) are always refused — \
                          for those, tell the user to run `cowboy secrets add`."
                .into(),
            parameters: schema_for::<RequestPathArgs>(),
        },
        ToolDef {
            name: TOOL_BLOCKED.into(),
            description: "Declare that you cannot proceed and need an external input \
                          (a decision, a dependency's artifact, access). Give a clear `reason` \
                          and optionally `waiting_on`. Use `unblock` once you can continue. This \
                          surfaces the session as blocked to the user / Ranch coordinator."
                .into(),
            parameters: schema_for::<BlockedArgs>(),
        },
        ToolDef {
            name: TOOL_UNBLOCK.into(),
            description: "Clear a previously-declared blocked state once you can proceed again."
                .into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: TOOL_PROPOSE_SCOPE_CHANGE.into(),
            description: "When running a Ranch workstream and the plan itself looks wrong \
                          (a workstream is missing, unnecessary, or misscoped), DO NOT edit the \
                          ranch plan — file a proposal here. It records a pending change the user \
                          reviews and approves/rejects; the plan only changes on approval. Use \
                          `change`: add_workstream (with workstream_id/title/goal/depends_on), \
                          remove_workstream (workstream_id), or note (a concern). Outside a ranch \
                          this is unavailable."
                .into(),
            parameters: schema_for::<ProposeScopeChangeArgs>(),
        },
        ToolDef {
            name: TOOL_FINAL.into(),
            description: "Finish the task. Provide a summary of what changed, what was \
                          validated, and any remaining risks or follow-up work."
                .into(),
            parameters: schema_for::<FinalArgs>(),
        },
        ToolDef {
            name: TOOL_ASK_USER.into(),
            description: "Ask the user a question when you are genuinely blocked and cannot \
                          proceed without their input. Whenever you are asking for a decision, \
                          give `options`: 2–4 choices, each a short `label` (a few words) with a \
                          `description` of what it means, and mark the one you recommend with \
                          `recommended: true`. The user picks one from a list — or types their \
                          own answer, which is always available, so never add an \"other\" \
                          option. Keep the question itself to a few sentences; put the detail in \
                          the descriptions."
                .into(),
            parameters: schema_for::<AskUserArgs>(),
        },
        ToolDef {
            name: TOOL_SUBAGENT.into(),
            description: "Delegate a focused, independent sub-task to a worker that shares this \
                          workspace/container. Describe the work by `category` (the kind of \
                          work — pick one your system prompt lists) and `effort` (tiny/small/\
                          medium/large/deep, judged on difficulty alone) — Cowboy routes it to the \
                          right model from the user's crew roster. Do NOT pick a model. Optionally set `agent` to adopt a named specialist \
                          definition from `.claude/agents/`/`.cowboy/agents/` (e.g. \
                          \"security-reviewer\"; discover with `cowboy agents list`). Include a \
                          `reason` and the `expected_artifact`. ASYNCHRONOUS: returns a job id \
                          immediately, NOT the worker's answer — keep working, and the result is \
                          delivered to you as a message when the job finishes (`jobs` to check, \
                          `wait` only when you have nothing else to do). Size each task to fit one \
                          worker's turn grant: split large work (per crate, module, or concern) and \
                          emit several calls in one message to run them in parallel."
                .into(),
            parameters: schema_for::<SubagentArgs>(),
        },
        ToolDef {
            name: TOOL_JOBS.into(),
            description: "List the background subagent jobs you have dispatched: state, how long \
                          each has been running, and its turn usage (used/granted, and the host \
                          ceiling it can be granted up to). Read-only — use it to decide whether \
                          to keep working, `wait`, or grant a worker more turns."
                .into(),
            parameters: schema_for::<JobsArgs>(),
        },
        ToolDef {
            name: TOOL_WAIT.into(),
            description: "Pause until a background subagent job reports — it finishes, or it asks \
                          for more turns. Use this ONLY when you have nothing else useful to do; \
                          results are delivered to you automatically either way, so waiting is \
                          never required to receive them. Returns early on a timeout so you are \
                          never stuck, and the user can always interrupt you."
                .into(),
            parameters: schema_for::<WaitArgs>(),
        },
        ToolDef {
            name: TOOL_JOB_REPLY.into(),
            description: "Answer a blocked subagent. Use `answer` (with your reply in \
                          `instructions`) when it asked a question about the work — it is asking \
                          you because you have the context it lacks. When it has spent its turn \
                          grant: `grant` gives it more turns, `redirect` gives it turns plus a \
                          different approach, `wrap_up` makes it write up what it has now, `stop` \
                          abandons it. For turn requests, judge the measured evidence in its \
                          report (files read, edits made, commands run, whether anything new \
                          happened) rather than its own optimism: no new files and no edits means \
                          more turns will not help."
                .into(),
            parameters: schema_for::<JobReplyArgs>(),
        },
        ToolDef {
            name: TOOL_REQUEST_TURNS.into(),
            description: "Ask your foreman for more turns, because the task is larger than the \
                          grant you were given. Report honestly: what you have established, what \
                          is left, the next concrete step, and how many more turns you need. You \
                          will be paused until the foreman answers — it may grant the turns, \
                          redirect you, tell you to write up what you have, or stop the work. \
                          Cowboy attaches measured evidence of your progress to the request, so \
                          an accurate report is in your interest."
                .into(),
            parameters: schema_for::<RequestTurnsArgs>(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Options may be bare labels or full objects; only the first recommendation
    /// stands, and blank labels are dropped.
    #[test]
    fn ask_options_accept_labels_and_objects() {
        let args: AskUserArgs = serde_json::from_value(serde_json::json!({
            "question": "Which route?",
            "options": [
                "plain",
                {"label": "rich", "description": "does more", "recommended": true},
                {"label": "also", "recommended": true},
                {"label": "  "}
            ]
        }))
        .unwrap();
        let c = args.choices();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].label, "plain");
        assert_eq!(c[1].description.as_deref(), Some("does more"));
        assert!(c[1].recommended);
        assert!(!c[2].recommended, "only one option may be recommended");
    }

    #[test]
    fn definitions_cover_the_tool_surface() {
        let names: Vec<_> = definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "shell",
                "read",
                "edit",
                "grep",
                "ls",
                "write",
                "proc",
                "memory",
                "plan",
                "artifact",
                "handoff",
                "decision",
                "request_path",
                "blocked",
                "unblock",
                "propose_scope_change",
                "final",
                "ask_user",
                "subagent",
                "jobs",
                "wait",
                "job_reply",
                "request_turns"
            ]
        );
    }

    #[test]
    fn nested_arg_schemas_are_inlined_not_ref() {
        // Models behind some OpenAI-compatible gateways (minimax-m3 on Fireworks)
        // don't resolve `$ref`/`$defs`, so nested array-of-object params must be
        // inlined or the model emits empty placeholders. Two tools have nested
        // struct args: `plan` (Vec<PlanStep>) and `edit` (Vec<EditSpecArgs>, the
        // batch form) — a batch edit that arrived as empty placeholders would be a
        // silent no-op, so both are checked.
        for (tool, fields) in [("plan", ["step", "status"]), ("edit", ["old", "new"])] {
            let schema = definitions()
                .into_iter()
                .find(|d| d.name == tool)
                .unwrap_or_else(|| panic!("{tool} tool present"))
                .parameters
                .to_string();
            assert!(
                !schema.contains("$ref") && !schema.contains("$defs"),
                "{tool} schema must be inlined, got: {schema}"
            );
            for f in fields {
                assert!(
                    schema.contains(&format!("\"{f}\"")),
                    "{tool} must expose the concrete `{f}` field: {schema}"
                );
            }
        }
        // And the batch array really is an array of objects, not of strings.
        let edit = definitions()
            .into_iter()
            .find(|d| d.name == "edit")
            .expect("edit tool present")
            .parameters;
        let items = &edit["properties"]["edits"]["items"];
        assert_eq!(
            items["type"], "object",
            "edits must be an array of objects, got: {items}"
        );
    }

    #[test]
    fn mcp_tool_is_conditional_and_well_formed() {
        // The `mcp` tool is NOT part of the always-on surface — it's added by the
        // agent loop only when MCP servers are connected.
        assert!(!definitions().iter().any(|d| d.name == TOOL_MCP));
        let d = mcp_definition();
        assert_eq!(d.name, "mcp");
        let schema = d.parameters.to_string();
        assert!(schema.contains("action"));
        assert!(schema.contains("server"));
        // Args parse as expected.
        let a: McpArgs =
            serde_json::from_str(r#"{"action":"call","server":"linear","tool":"create_issue","arguments":{"title":"x"}}"#)
                .unwrap();
        assert_eq!(a.action, "call");
        assert_eq!(a.server.as_deref(), Some("linear"));
    }

    #[test]
    fn shell_args_parse_from_json() {
        let a: ShellArgs = serde_json::from_str(r#"{"command":"ls -la"}"#).unwrap();
        assert_eq!(a.command, "ls -la");
        assert!(a.cwd.is_none());
    }

    #[test]
    fn shell_schema_declares_command() {
        let schema = schema_for::<ShellArgs>();
        let s = schema.to_string();
        assert!(s.contains("command"));
    }

    /// Every name in the wrap-up allowlist must be a real tool.
    ///
    /// A typo here fails in the worst possible direction: the name silently does not
    /// match, the tool is denied, and if it were `final` every wrap-up would deadlock
    /// — the worker told to report would have no way to report. Cheap to pin.
    #[test]
    fn the_wrap_up_allowlist_names_only_real_tools() {
        let names: Vec<String> = definitions().into_iter().map(|d| d.name).collect();
        for allowed in WRAP_UP_ALLOWED {
            assert!(
                names.iter().any(|n| n == allowed),
                "{allowed:?} is in WRAP_UP_ALLOWED but is not a tool"
            );
        }
        // The point of the gate: a worker out of budget reports, it does not keep
        // digging or editing.
        for denied in [
            TOOL_SHELL,
            TOOL_READ,
            TOOL_GREP,
            TOOL_LS,
            TOOL_EDIT,
            TOOL_WRITE,
            TOOL_SUBAGENT,
            TOOL_REQUEST_TURNS,
            TOOL_ASK_USER,
        ] {
            assert!(
                !allowed_when_wrapping_up(denied),
                "{denied:?} must not be callable while wrapping up"
            );
        }
        // And the report itself must be, or the gate is a deadlock.
        assert!(allowed_when_wrapping_up(TOOL_FINAL));
        // A foreman can still collect subagents it is waiting on; `final` refuses
        // while jobs are in flight, so denying these would wedge that path.
        assert!(allowed_when_wrapping_up(TOOL_WAIT));
        assert!(allowed_when_wrapping_up(TOOL_JOBS));
    }

    #[test]
    fn snapshot_tool_definitions_json() {
        // The exact tool payload sent to the model is reviewed deliberately.
        let defs: Vec<_> = definitions()
            .into_iter()
            .map(|d| {
                serde_json::json!({
                    "name": d.name,
                    "description": d.description,
                    "parameters": d.parameters,
                })
            })
            .collect();
        insta::assert_json_snapshot!(defs);
    }
}
