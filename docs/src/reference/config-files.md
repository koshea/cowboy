# Config files

A map of every file Cowboy reads or writes. For annotated examples of the three
editable config files, see [Configuration](../getting-started/configuration.md).

## Host-owned (home dir — never mounted, never in a project)

| Path | Purpose |
|------|---------|
| `~/.config/cowboy/providers.yaml` | Provider endpoints + API keys (`0600`). The agent can't reach this. |
| `~/.config/cowboy/models.yaml` | User-level model definitions + default. |
| `~/.config/cowboy/crew.yaml` | [Crew](../using/crew.md) roster — delegated-work routing by category/effort. |
| `~/.config/cowboy/crew-history.jsonl` | Recorded delegation outcomes (append-only; powers `cowboy crew usage`). |
| `~/.config/cowboy/mcp.yaml` | [MCP](../how-to.md) server definitions (host-owned; the agent can call but not edit). |
| `~/.config/cowboy/web.yaml` | [Web UI](../using/web.md) setting + bearer token (`0600`). |
| `~/.config/cowboy/skills/` | User-level [skills](../using/skills-and-subagents.md). |
| `~/.config/cowboy/approvals/<project>.json` | Persisted [network approvals](../security/network.md), per project (`0600`). Host-side **on purpose**: in the workspace the agent could widen its own egress by writing the file. |
| `~/.config/cowboy/grants/` | Persisted path [grants](../security/model.md), per project + global (`0600`, dir `0700`). Host-side for the same reason. |
| `~/.config/cowboy/mcp-trust/<project>.json` | Which `.mcp.json` server set you approved with [`cowboy mcp trust`](../how-to.md), pinned so a later edit goes stale. Host-side: the agent must not be able to trust servers on its own. |
| `~/.config/cowboy/tips/` | Marks for one-shot hints already shown. Host-side so a repository cannot suppress or resurrect them. |

## Per-project (`.cowboy/`)

| Path | Mounted? | Purpose |
|------|----------|---------|
| `.cowboy/security.yaml` | **masked** | Sandbox mounts + limits, networks, policy, secrets (host-owned). |
| `.cowboy/agent.yaml` | yes | Non-security agent behavior, processes, command aliases. |
| `.cowboy/models.yaml` | **masked** | Project model definitions (no credentials). |
| `.mcp.json` | — | Project-declared [MCP](../how-to.md) servers (trust-gated; the format other MCP clients use). |
| `.cowboy/skills/` | yes | Project skills. |
| `.cowboy/sessions/<id>/` | — | Per-session logs (gitignored). |
| `.cowboy/ranches/<id>/` | — | Ranch plans + promoted artifacts + proposals (committed). |

## Session directory (`.cowboy/sessions/<id>/`, gitignored)

| File | Purpose |
|------|---------|
| transcript / command logs / diff | The raw run. |
| `artifacts/` + `artifacts.jsonl` | Published outputs. |
| `handoff.md` | Headline summary (auto-generated if not published). |
| `lifecycle.jsonl` | Semantic events (consumed by the Ranch coordinator). |
| `decisions.jsonl` | Recorded decisions. |
| `events.jsonl` | UI/journal events (for attach/replay). |

## Ranch directory (`.cowboy/ranches/<id>/`, committed)

| Path | Purpose |
|------|---------|
| `ranch.yaml` | The plan — the source of truth. |
| `artifacts/<workstream>/` | Promoted outputs of completed workstreams. |
| `proposals/<pid>.yaml` | Scope-change proposals (audit trail). |

## Daemon (per-user)

| Path | Purpose |
|------|---------|
| `$XDG_RUNTIME_DIR/cowboy/` | Daemon + worker sockets, lock (`0700`; the sockets are `0600` and peer-uid checked — see [the boundary](../security/model.md)). |
| `$XDG_STATE_HOME/cowboy/daemon/state.json` | Session registry + leases. |
| `$XDG_STATE_HOME/cowboy/jobs/<parent>/<job>/` | The parent↔subagent control channel (`0700`, files `0600`): turn requests, verdicts, [questions and answers](../using/crew.md#a-worker-can-ask-a-question). Deliberately **not** in `.cowboy/`, which is writable from inside the sandbox — a verdict file there could be written by sandboxed content and would then steer another agent. |
| `$XDG_CACHE_HOME/cowboy/home/<repo-key>/` | The sandboxed agent's `HOME`, bound at `/home/agent` (`0700`). Per-repository, so all worktrees share one warm cache; safe to delete, at the cost of re-downloading. Deliberately **not** `.cowboy/home` in the workspace — see [sandbox decisions](../security/sandbox-decisions.md#the-agents-home-does-not-belong-in-the-workspace). |

## Unknown keys are an error

Every config file above is parsed strictly: a key Cowboy does not recognise fails the
load and names itself in the error. This is not pedantry — the alternative is that a
typo *silently* leaves that section at its defaults. A misspelt `netwrok_policy:` used
to mean the deny rules under it did not exist, and the only symptom was a sandbox
behaving as though it had never been configured.

Two keys are retired rather than unknown, and are declared so that they can be
handled deliberately:

- `container:` in `security.yaml` → **refused by name**, pointing at `sandbox:`.
  Dropping it would take every mount under it with it.
- `planner:` in `crew.yaml` → **accepted and ignored**, since the foreman is now the
  selected model. Dropping it costs nothing, and it is not written back out.
