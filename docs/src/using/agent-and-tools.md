# The agent & its tools

The agent loop calls an OpenAI-compatible model with a fixed tool surface. Some
tools run **inside the container** (the sandbox); others are **host-handled** by
the loop (the agent can't reach the host directly, so the loop does it).

## Tool surface

| Tool | Where | What it does |
|------|-------|--------------|
| `shell` | container | Run a command in the agent container; output is streamed back. All other Cowboy CLIs (`patch`, `proc`, `skill`) are invoked *through* `shell`. |
| `read` / `edit` / `write` | container | File operations in the workspace. |
| `memory` | host | Save/recall durable project memory. |
| `plan` | host | Maintain a working to-do plan, surfaced in the UI; drives lifecycle events. |
| `artifact` | host | Publish a named output (contract, summary, review, …) into the session's artifact store. |
| `handoff` | host | Write a structured handoff summary (`handoff.md`) at the end of a session. |
| `decision` | host | Record a decision (question, options, choice, rationale) durably. |
| `blocked` / `unblock` | host | Declare/clear a "cannot proceed" state, surfaced to the user and the Ranch coordinator. |
| `propose_scope_change` | host | (Ranch only) File a pending change to the ranch plan for the user to approve — the agent never edits the plan directly. |
| `final` | — | Finish the current *turn* with a summary. |
| `ask_user` | host | Ask the user a question, optionally with selectable options. |
| `subagent` | host | Delegate a focused sub-task to a fresh subagent in the same container. |

The exact, current list is asserted by a test and rendered in the
[CLI reference](../reference/cli.md) companion; adding a tool follows the pattern
documented in `AGENTS.md`.

## The container environment

The agent runs in a **batteries-included** image (`docker/agent.Dockerfile`):
bash, git, [`gh`](https://cli.github.com/), ripgrep/fd/jq, Python, Node (npm +
pnpm), Go, a Rust toolchain, and common build/db client tooling. The workspace is
mounted at `/workspace`; the agent runs as your (non-root) host user.

> Working in a **git worktree**? Cowboy detects it and also mounts the main
> repository's git directory into the container, so `git` (status/diff/log/commit)
> works even though the worktree's `.git` points outside `/workspace`.

### Managing dependencies with mise (recommended)

[mise](https://mise.jdx.dev/) is the **preferred way to manage per-project dev
dependencies** (language runtimes, CLIs, env vars) in the container. It's
installed in the image, and:

- The container sets **`MISE_ENV=devcontainer`**, so a `[env]`/task config can
  branch on the devcontainer environment (e.g. `mise.devcontainer.toml`).
- When the workspace has a mise config (`mise.toml`, `.mise.toml`,
  `.config/mise/config.toml`, `.tool-versions`, …), Cowboy runs **`mise install`
  automatically at launch** — so a freshly-created worktree comes up with its
  declared toolchain ready, no manual step.
- The workspace is trusted automatically inside the container
  (`MISE_TRUSTED_CONFIG_PATHS=/workspace`), and mise's shims are on `PATH` for
  both the agent's commands and an interactive `cowboy shell`.

Commit a mise config to your repo and the agent gets a consistent, reproducible
toolchain every session.

## Context management

Conversation history is kept within the model's window using `tiktoken`-based
token counting (oldest history is pruned, or compacted into a summary); command
output is additionally byte-capped (`agent.max_command_output_bytes`). Token and
estimated-cost totals are tracked per session, with optional budgets.

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
