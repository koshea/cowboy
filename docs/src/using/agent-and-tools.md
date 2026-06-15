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
