# Introduction

**Cowboy** is an opinionated local coding agent that lets the AI run wild inside a
Docker-contained development environment, while the **host** enforces security at
the container and network layer.

> The agent can run wild because the runtime owns the corral.

The central principle, repeated throughout these docs: **the agent is not part of
the security boundary.** Controls are enforced by Docker, host-owned
configuration, and a Cowboy-controlled network gateway — never by prompting the
model. If a chapter ever seems to rely on the model behaving, that's a bug in the
docs (or the code).

## What you get

- **A contained agent.** The agent works in a Docker container with your project
  mounted at `/workspace`; host-owned config and credentials are never reachable
  from inside.
- **A real network boundary.** Outbound traffic is forced through a sole-egress
  gateway that enforces an allow/deny/ask policy by routing, not by asking the
  model. See [Network gateway](security/network.md).
- **A conversational TUI** that streams the agent's work, with live approval
  prompts for network access.
- **Sessions & a daemon** (`cowboyd`) that supervise worker processes, track
  worktree leases, and let you attach/detach/replay.
- **A configurable crew** — your selected model (the foreman) delegates work by *kind* (category
  + effort) and Cowboy routes each sub-task to the right model from your roster,
  running independent work in parallel. See [The crew](using/crew.md).
- **Ranch Plans** — the headline feature: split a large task into coordinated,
  dependency-aware workstreams, each its own session in its own worktree, with a
  coordinator that advances the plan and pauses for your sign-off where it matters.

## How to read this

- New here? Start with [Installation](getting-started/installation.md) and the
  [Quick start](getting-started/quickstart.md).
- Want to understand the guarantees? Read the [Security model](security/model.md)
  and [Network gateway](security/network.md).
- Orchestrating big work? Jump to [Ranch Plans](ranch/overview.md).
- Working on Cowboy itself? See [Contributing](contributing.md) (and `AGENTS.md`
  at the repo root).

## Platform support

- **Linux** — supported (the current target).
- **macOS** — planned (Docker Desktop networking for the gateway needs work).
- **Windows** — out of scope.
