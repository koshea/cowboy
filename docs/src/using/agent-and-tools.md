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

Everything sent to the model competes for one window, and the loop's job is to keep the
request inside it while losing as little as possible.

**The budget.** The window is not all yours: room is reserved for the model's reply, for
the tool schemas (~3.6k tokens, sent on *every* request), and for a small headroom
floor. What is left is the conversation's budget. If the window cannot even hold the
reserve, Cowboy says so — naming the window, the model's `max_tokens` and the schema
cost — rather than letting the request fail at the provider with an error that explains
none of that.

**`/context`** shows where you stand at any time, including mid-turn:

```
context  84,500/160,000 tokens of the conversation budget (52%)
         window 200,000 · reserved 40,000 for the reply, tool schemas and headroom
         largest first:
           tool results             41,000  █████████
           model reasoning          22,000  █████
           assistant messages       12,500  ██
           tool schemas              3,650
```

Grouped by what produced it, because the useful question is which *kind* of thing is
filling the window.

**What is shed, in order of cheapness.** Reasoning models return their thinking, and it
is sent back on every subsequent request to keep them on plan across tool calls — but
only the last couple of turns need it, so older reasoning is dropped before each call.
This is free and happens first, which often means there is nothing left to compact. Then
every tool result is capped (`agent.max_command_output_bytes`, 60 KB) — all of them,
including subagent answers and MCP responses. Only if the conversation still overflows
does Cowboy compact: the oldest whole turns are folded into a model-written summary
(itself capped, so a fold always shrinks). Dropping history without summarizing is the
last resort, and it says so each time it happens, with a count.

**What never goes.** The system prompt and your current task statement. The task is
tracked by content, so it survives every fold and prune wherever it sits — including
after `--resume`, where the previous session's transcript sits in front of it. A resume
loads at most half the budget, newest first, so continuing an old session cannot blow
the window on the first request.

If a reasoning model burns its whole output budget thinking and returns no answer
or tool call, Cowboy warns that its `max_tokens` may be too low and then recovers
rather than ending the turn. It retries with a directive to answer now, and asks the
provider for **minimal reasoning effort** on that retry — telling a reasoning model
not to think is advice it can ignore, so the knob the provider honours is turned as
well. When the provider returned the cut-off reasoning, it is first distilled into
conclusions-so-far so the retry builds on them instead of re-deriving them; when it
did not — many providers bill reasoning tokens without ever sending the text — the
retry goes ahead anyway, because the model still has the whole transcript.

Recovery is bounded (`2` attempts). Only after those are spent does the turn end,
with an `[incomplete]` result naming the two levers you have: raise `max_tokens` or
lower `reasoning_effort`. The low-effort request lasts for the retry only, so a model
that recovers keeps its normal reasoning for the rest of the session. Both the
compaction and recovery summaries use the optional
[`summarizer`](../getting-started/configuration.md) model when configured,
falling back to the main model otherwise, and always request minimal reasoning —
summarizing is mechanical, and a model that just truncated while thinking would
otherwise do the same on the summary and come back empty.

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
