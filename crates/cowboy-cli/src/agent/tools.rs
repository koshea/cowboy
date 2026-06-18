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
pub const TOOL_READ: &str = "read";
pub const TOOL_EDIT: &str = "edit";
pub const TOOL_WRITE: &str = "write";
pub const TOOL_MEMORY: &str = "memory";
pub const TOOL_PLAN: &str = "plan";
pub const TOOL_ARTIFACT: &str = "artifact";
pub const TOOL_HANDOFF: &str = "handoff";
pub const TOOL_BLOCKED: &str = "blocked";
pub const TOOL_UNBLOCK: &str = "unblock";
pub const TOOL_DECISION: &str = "decision";
pub const TOOL_PROPOSE_SCOPE_CHANGE: &str = "propose_scope_change";
pub const TOOL_PROPOSE_RANCH: &str = "propose_ranch";
/// Conditional: added only when ≥1 MCP server is enabled (see [`mcp_definition`]).
pub const TOOL_MCP: &str = "mcp";

/// Arguments for the `shell` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// The shell command to run inside the container (executed with `sh -lc`).
    pub command: String,
    /// Optional working directory (defaults to the container workdir).
    #[serde(default)]
    pub cwd: Option<String>,
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
    /// A question for the user when the agent is genuinely blocked.
    pub question: String,
    /// Optional suggested answers to present as a pick-list. The user may still
    /// type a free-form answer ("other").
    #[serde(default)]
    pub options: Option<Vec<String>>,
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
    /// The KIND of work, so Cowboy routes it to the right crew model. One of:
    /// general, exploration, backend, frontend, tests, docs, debugging,
    /// refactor, e2e. Defaults to `general`. Do NOT name a model — routing is the
    /// user's crew roster.
    #[serde(default)]
    pub category: Option<String>,
    /// How hard the task is: tiny, small, medium, large, or deep. Defaults to
    /// `medium`. Used with `category` to pick the model.
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

/// Arguments for the `edit` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditArgs {
    /// Path to the file to edit (workspace-relative).
    pub path: String,
    /// Exact text to replace. Must match a unique span unless `replace_all`.
    pub old: String,
    /// Replacement text.
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

/// Arguments for the `write` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// Path to write (workspace-relative). Parent directories are created.
    pub path: String,
    /// Full file contents (overwrites any existing file).
    pub content: String,
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

/// One workstream in a proposed Ranch Plan (see [`ProposeRanchArgs`]).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RanchWorkstreamArg {
    /// Short, unique slug id (e.g. `schema`, `api`).
    pub id: String,
    /// What this workstream should accomplish.
    pub goal: String,
    /// Display title (defaults to the id).
    #[serde(default)]
    pub title: Option<String>,
    /// Ids of workstreams that must finish before this one starts (forms a DAG).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Artifacts this workstream is expected to publish (names, not paths).
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
    /// Human-readable acceptance criteria.
    #[serde(default)]
    pub acceptance: Vec<String>,
}

/// Arguments for the `propose_ranch` tool: a full multi-workstream decomposition.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ProposeRanchArgs {
    /// Title for the ranch plan.
    pub title: String,
    /// The overall goal the ranch achieves.
    pub goal: String,
    /// The workstreams, with `depends_on` wiring them into a dependency DAG.
    pub workstreams: Vec<RanchWorkstreamArg>,
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
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| serde_json::json!({}))
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
            description: "Run a shell command inside the container and observe its output. \
                          Use this for builds, tests, git, and cowboy CLIs like `cowboy patch`. \
                          For reading or editing files, prefer the `read`/`edit`/`write` tools."
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
                          be unique unless `replace_all` is set. Prefer this over `sed`/heredocs \
                          for edits — it is precise and fails loudly if `old` is missing or \
                          ambiguous."
                .into(),
            parameters: schema_for::<EditArgs>(),
        },
        ToolDef {
            name: TOOL_WRITE.into(),
            description: "Create a new file or overwrite an existing one with the given content. \
                          Parent directories are created. Prefer this over `echo >`/heredocs."
                .into(),
            parameters: schema_for::<WriteArgs>(),
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
            name: TOOL_PROPOSE_RANCH.into(),
            description: "Propose a multi-workstream Ranch Plan: decompose a large goal into \
                          independent, parallelizable workstreams wired by `depends_on` into a \
                          DAG, each with a goal, expected artifacts, and acceptance criteria. \
                          Use ONLY when explicitly asked to plan/decompose a ranch — it writes a \
                          draft ranch.yaml for the user to review and does NOT start any work or \
                          edit code. Call it once with the full decomposition."
                .into(),
            parameters: schema_for::<ProposeRanchArgs>(),
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
                          proceed without their input. Provide `options` (2–4 short choices) when \
                          the answer is a clear pick — the user gets a selectable list and can \
                          still type their own answer."
                .into(),
            parameters: schema_for::<AskUserArgs>(),
        },
        ToolDef {
            name: TOOL_SUBAGENT.into(),
            description: "Delegate a focused, independent sub-task to a worker that shares this \
                          workspace/container. Describe the work by `category` (the kind: tests, \
                          exploration, frontend, review, …) and `effort` (tiny/small/medium/large/\
                          deep) — Cowboy routes it to the right model from the user's crew roster. \
                          Do NOT pick a model. Optionally set `agent` to adopt a named specialist \
                          definition from `.claude/agents/`/`.cowboy/agents/` (e.g. \
                          \"security-reviewer\"; discover with `cowboy agents list`). Include a \
                          `reason` and the `expected_artifact`. Returns the worker's final summary. \
                          Prefer small, well-scoped tasks; emit several calls in one message to run \
                          them in parallel."
                .into(),
            parameters: schema_for::<SubagentArgs>(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_cover_the_tool_surface() {
        let names: Vec<_> = definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "shell",
                "read",
                "edit",
                "write",
                "memory",
                "plan",
                "artifact",
                "handoff",
                "decision",
                "blocked",
                "unblock",
                "propose_scope_change",
                "propose_ranch",
                "final",
                "ask_user",
                "subagent"
            ]
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
