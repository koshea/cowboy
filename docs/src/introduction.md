# Introduction

**Cowboy** is an opinionated local coding agent that lets the AI run wild in a
sandbox built from your own machine, while the **host** enforces security at the
kernel and network layer.

> The agent can run wild because the runtime owns the corral.

The central principle, repeated throughout these docs: **the agent is not part of
the security boundary.** Controls are enforced by Linux namespaces, Landlock,
seccomp, host-owned configuration, and a policy engine that runs in the host
process — never by prompting the model. If a chapter ever seems to rely on the
model behaving, that's a bug in the docs (or the code).

## What you get

- **A confined agent that still has your toolchain.** Your project is writable,
  `/usr` and `/opt` are read-only, and the rest of the machine is simply absent —
  so the agent builds with the compilers and runtimes you actually installed,
  with no image to pull or keep in sync. Host-owned config and credentials are
  unreachable by construction.
- **Access you can widen as you go.** Need the agent to see a sibling repository?
  It asks, or you run `cowboy grant <path>`, and the next command sees it — no
  restart, no config edit. Credential stores are refused however you ask.
- **A real network boundary.** The sandbox's network namespace is connected to
  nothing, so a broken ruleset means the agent reaches *nothing* rather than
  everything. Traffic is made visible to a host-side policy engine that enforces
  allow/deny/ask. See [Network egress](security/network.md).
- **A conversational TUI** that streams the agent's work, with live approval
  prompts for network access.
- **A web UI** to drive sessions from a browser — keep coding from your phone over
  Tailscale, alongside or instead of the terminal. See [The web UI](using/web.md).
- **Sessions & a daemon** (`cowboyd`) that supervise worker processes, track
  worktree leases, and let you attach/detach/replay — from the TUI or the browser.
- **A configurable crew** — your selected model (the foreman) delegates work by *kind* (category
  + effort) and Cowboy routes each sub-task to the right model from your roster,
  running independent work in parallel. See [The crew](using/crew.md).
- **Ranch Plans** — the headline feature: split a large task into coordinated,
  dependency-aware workstreams, each its own session in its own worktree, with a
  coordinator that advances the plan and pauses for your sign-off where it matters.

## How to read this

- New here? Start with [Installation](getting-started/installation.md) and the
  [Quick start](getting-started/quickstart.md).
- Want to understand the guarantees? Read [The boundary](security/model.md) and
  [Network egress](security/network.md). For *why* each mechanism was chosen, and
  the evidence behind it, see
  [Sandbox design decisions](security/sandbox-decisions.md).
- Orchestrating big work? Jump to [Ranch Plans](ranch/overview.md).
- Working on Cowboy itself? See [Contributing](contributing.md) (and `AGENTS.md`
  at the repo root).

## Platform support

**Linux only.** The sandbox is namespaces, Landlock, seccomp and nftables — kernel
features with no equivalent elsewhere, and there is no container or VM in the
design to borrow one from. macOS support went with the container, deliberately.

Cowboy was built against a current kernel (Landlock ABI 6+, Linux 6.10 or newer).
`cowboy doctor` checks each prerequisite by performing it and names what to change
if something is missing.
