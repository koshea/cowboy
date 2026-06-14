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
            vec!["shell", "read", "edit", "write", "final", "ask_user", "subagent"]
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
