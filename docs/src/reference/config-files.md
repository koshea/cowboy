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

## Per-project (`.cowboy/`)

| Path | Mounted? | Purpose |
|------|----------|---------|
| `.cowboy/security.yaml` | **masked** | Sandbox mounts + limits, networks, policy, secrets (host-owned). |
| `.cowboy/agent.yaml` | yes | Non-security agent behavior, processes, command aliases. |
| `.cowboy/models.yaml` | **masked** | Project model definitions (no credentials). |
| `.mcp.json` | — | Project-declared [MCP](../how-to.md) servers (trust-gated; the format other MCP clients use). |
| `.cowboy/approvals.json` | — | Persisted project/global network approvals. |
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
