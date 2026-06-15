# Config files

A map of every file Cowboy reads or writes. For annotated examples of the three
editable config files, see [Configuration](../getting-started/configuration.md).

## Host-owned (home dir — never mounted, never in a project)

| Path | Purpose |
|------|---------|
| `~/.config/cowboy/providers.yaml` | Provider endpoints + API keys (`0600`). The agent can't reach this. |
| `~/.config/cowboy/models.yaml` | User-level model definitions + default. |
| `~/.config/cowboy/skills/` | User-level [skills](../using/skills-and-subagents.md). |

## Per-project (`.cowboy/`)

| Path | Mounted? | Purpose |
|------|----------|---------|
| `.cowboy/security.yaml` | **masked** | Container, mounts, networks, policy, secrets (host-owned). |
| `.cowboy/agent.yaml` | yes | Non-security agent behavior, processes, command aliases. |
| `.cowboy/models.yaml` | **masked** | Project model definitions (no credentials). |
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
| `$XDG_RUNTIME_DIR/cowboy/` | Daemon + worker sockets, lock. |
| `$XDG_STATE_HOME/cowboy/daemon/state.json` | Session registry + leases. |
