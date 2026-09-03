# The agent & its tools

The agent loop calls an OpenAI-compatible model with a fixed tool surface. Some
tools run **inside the sandbox**; others are **host-handled** by the loop (the
agent can't reach the host directly, so the loop does it).

## Tool surface

| Tool | Where | What it does |
|------|-------|--------------|
| `shell` | sandbox | Run a command in the sandbox; output is streamed back. All other Cowboy CLIs (`patch`, `proc`, `skill`) are invoked *through* `shell`. |
| `read` / `edit` / `write` | sandbox | File operations in the workspace. |
| `memory` | host | Save/recall durable project memory. |
| `plan` | host | Maintain a working to-do plan, surfaced in the UI; drives lifecycle events. |
| `artifact` | host | Publish a named output (contract, summary, review, …) into the session's artifact store. |
| `handoff` | host | Write a structured handoff summary (`handoff.md`) at the end of a session. |
| `decision` | host | Record a decision (question, options, choice, rationale) durably. |
| `blocked` / `unblock` | host | Declare/clear a "cannot proceed" state, surfaced to the user and the Ranch coordinator. |
| `propose_scope_change` | host | (Ranch only) File a pending change to the ranch plan for the user to approve — the agent never edits the plan directly. |
| `request_path` | host | Ask the user for access to a host path outside the workspace. Approved paths apply to the *next* command. Credential stores are always refused. |
| `final` | — | Finish the current *turn* with a summary. |
| `ask_user` | host | Ask the user a question, optionally with selectable options. |
| `subagent` | host | Delegate a focused sub-task to a fresh subagent in the same sandbox. |

The exact, current list is asserted by a test and rendered in the
[CLI reference](../reference/cli.md) companion; adding a tool follows the pattern
documented in `AGENTS.md`.

## The sandbox environment

The agent gets **your** toolchain. `/usr` and `/opt` are exposed read-only, so
whatever compilers, runtimes and CLIs you have installed are what the agent runs,
at your versions — there is no image to build, pull, or keep in step. Your project
is writable at `/workspace`; the rest of the machine is absent.

Run `cowboy sandbox plan` to see the exact list for your machine.

> Working in a **git worktree**? Cowboy detects it and also exposes the main
> repository's git directory, so `git` (status/diff/log/commit) works even though
> the worktree's `.git` points outside `/workspace`.

If a command fails because something outside the project is missing, the agent can
call `request_path` to ask for it, or you can run `cowboy grant <path>`. Either way
the next command sees it, with no restart. Credential stores are refused whatever
the reason given — use `cowboy secrets add` for those.

### Managing dependencies with mise (recommended)

[mise](https://mise.jdx.dev/) is the **preferred way to manage per-project dev
dependencies** (language runtimes, CLIs, env vars) that the host does not already
provide. Install it on the host and:

- When the workspace has a mise config (`mise.toml`, `.mise.toml`,
  `.config/mise/config.toml`, `.tool-versions`, …), Cowboy runs **`mise install`
  automatically at launch** — so a freshly-created worktree comes up with its
  declared toolchain ready, no manual step.
- The workspace is trusted automatically
  (`MISE_TRUSTED_CONFIG_PATHS=/workspace`), and mise's shims are on `PATH` for
  both the agent's commands and an interactive `cowboy shell`.

Commit a mise config to your repo and the agent gets a consistent, reproducible
toolchain every session.

## Context management

Conversation history is kept within the model's window using `tiktoken`-based
token counting (oldest history is pruned, or compacted into a summary); command
output is additionally byte-capped (`agent.max_command_output_bytes`). Token and
estimated-cost totals are tracked per session, with optional budgets.

If a reasoning model burns its whole output budget thinking and returns no answer
or tool call, Cowboy warns that its `max_tokens` may be too low, then tries to
recover: it summarizes the truncated reasoning and retries the turn with a
directive to act on those conclusions rather than re-deriving them. Both the
compaction and recovery summaries use the optional
[`summarizer`](../getting-started/configuration.md) model when configured,
falling back to the main model otherwise.

## What a session records

Under `.cowboy/sessions/<id>/`:

- **transcript / command logs / diff** — the raw run.
- **`artifacts/` + `artifacts.jsonl`** — published outputs (the `artifact` tool).
- **`handoff.md`** — the session's headline summary (auto-generated if the agent
  didn't publish one).
- **`lifecycle.jsonl`** — semantic events (plan steps, artifacts, blocked/
  unblocked, decisions, completion) consumed by the Ranch coordinator.
- **`decisions.jsonl`** — recorded decisions.

These outputs are what makes [Ranch Plans](../ranch/overview.md) coordinate
through artifacts rather than chat.
