//! The agent's tool surface — deliberately minimal: `shell`, `final`,
//! `ask_user`. Everything cowboy-specific is a CLI the agent calls *through*
//! `shell` (e.g. `cowboy patch`), never a built-in tool.

use cowboy_core::model::ToolDef;
use schemars::JsonSchema;
use serde::Deserialize;

pub const TOOL_SHELL: &str = "shell";
pub const TOOL_FINAL: &str = "final";
pub const TOOL_ASK_USER: &str = "ask_user";

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

fn schema_for<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| serde_json::json!({}))
}

/// The tool definitions advertised to the model.
pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: TOOL_SHELL.into(),
            description: "Run a shell command inside the container and observe its output. \
                          Use this for all work, including cowboy CLIs like `cowboy patch`."
                .into(),
            parameters: schema_for::<ShellArgs>(),
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_cover_the_minimal_surface() {
        let names: Vec<_> = definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["shell", "final", "ask_user"]);
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
