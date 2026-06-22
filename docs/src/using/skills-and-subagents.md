# Skills, agents & subagents

## Skills

**Skills** are reusable instructions in a `SKILL.md` (YAML frontmatter
`name`/`description`, then a markdown body). They're discovered, in precedence
order, from `.cowboy/skills/` and **`.claude/skills/`** in the project, then
`~/.config/cowboy/skills/` and **`~/.claude/skills/`** globally — so Cowboy and
Claude Code users share the same skills in a repo.

The agent finds them with `cowboy skill list` and pulls a skill's instructions
into context with `cowboy skill show <name>`. Both run through the `shell` tool —
skills are a CLI surface, not a built-in tool, so humans and CI use the same
commands.

## Agents

**Agent definitions** are named specialist personas — a Markdown file with
frontmatter (`name`, `description`, optional `model`) whose body is the agent's
system prompt. They're discovered from `.cowboy/agents/` and **`.claude/agents/`**
(project), then the same `~/.config/cowboy/` and `~/.claude/` globals — again
shared with Claude Code.

`cowboy agents list` / `cowboy agents show <name>` surface them. To have a
subagent **adopt** one, pass `agent: <name>` to the `subagent` tool: its
instructions are prepended so the worker takes on that persona. The model is
still chosen by the [crew](crew.md) (category + effort), not the agent's
frontmatter — Cowboy's routing stays in charge.

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

### Watching a subagent

Each running subagent streams its own live journal, so you can look inside one
instead of waiting for its final answer. They appear in the background pane as the
foreman fans them out; to watch one:

- **TUI** — press **Ctrl-C → `w`** to open a subagent's live output (press `w`
  again to cycle through them; **Esc** returns to the main session).
- **Web UI** — tap a subagent chip above the transcript to open its live view
  (read-only); a finished subagent replays its recorded transcript.

This is the fast way to see *why* a subagent is slow or stuck (e.g. a hung
command) rather than guessing from a frozen timer.
