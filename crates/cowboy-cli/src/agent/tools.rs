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

fn schema_for<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| serde_json::json!({}))
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
            name: TOOL_FINAL.into(),
            description: "Finish the task. Provide a summary of what changed, what was \
                          validated, and any remaining risks or follow-up work."
                .into(),
            parameters: schema_for::<FinalArgs>(),
        },
        ToolDef {
            name: TOOL_ASK_USER.into(),
            description: "Ask the user a question when you are genuinely blocked and cannot \
                          proceed without their input."
                .into(),
            parameters: schema_for::<AskUserArgs>(),
        },
        ToolDef {
            name: TOOL_SUBAGENT.into(),
            description: "Delegate a large, independent sub-task to a fresh subagent that shares \
                          this workspace/container. Returns the subagent's final summary. Use for \
                          focused work you want handled with its own context."
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
                "shell", "read", "edit", "write", "memory", "plan", "artifact", "handoff", "final",
                "ask_user", "subagent"
            ]
        );
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
