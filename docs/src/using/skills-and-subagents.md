# Skills & subagents

## Skills

**Skills** are reusable instructions in `.cowboy/skills/<name>/SKILL.md` (YAML
frontmatter `name`/`description`, then a markdown body), discovered from the
project and from `~/.config/cowboy/skills/`.

The agent finds them with `cowboy skill list` and pulls a skill's instructions
into context with `cowboy skill show <name>`. Both run through the `shell` tool —
skills are a CLI surface, not a built-in tool, so humans and CI use the same
commands.

## Subagents

**Subagents** let the agent delegate a focused sub-task via the `subagent` tool.
It recursively invokes `cowboy` in one-shot mode, reusing the same container (so
the subagent shares the workspace and gateway), and folds the subagent's final
answer back into the parent's context. Nesting is depth-limited to prevent runaway
recursion.

Use a subagent for independent, well-scoped work you want handled with its own
context budget — distinct from a [Ranch](../ranch/overview.md) workstream, which
is a full session in its own worktree/branch coordinated across the dependency
graph.
