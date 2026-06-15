# Sessions & the daemon

A **session** is one agent run against a worktree. Sessions are coordinated by a
local daemon, **`cowboyd`**, which is started automatically when needed.

## How a session works

1. `cowboy` loads host-owned `security.yaml` (never mounted into the container).
2. It builds/starts the agent container with the project mounted at `/workspace`
   and the host-owned config **masked**.
3. With isolation enabled (default), it brings up the sole-egress network gateway
   and forces the agent's only route out through it (see
   [Network gateway](../security/network.md)).
4. The agent loop calls an OpenAI-compatible model with a tool surface (see
   [The agent & its tools](agent-and-tools.md)).
5. Shell commands run in the container; output is streamed to the UI and fed back
   to the model. The session is logged under `.cowboy/sessions/<id>/`.

## The daemon (`cowboyd`)

`cowboyd` is the control plane. It does **not** host agent loops or sit in the
event-stream data path — sessions run as separate worker processes (`cowboy
x-session-worker`). The daemon:

- maintains a persistent **registry** of sessions and their status;
- manages **worktree leases** — at most one writable (`Exclusive`) session per
  worktree, so two agents never fight over the same files;
- creates **git worktrees/branches** on request;
- supervises workers, marks crashed/abandoned ones `Stale`, and reaps them;
- mediates a small **message bus** between sessions.

It listens on a per-user Unix socket under `$XDG_RUNTIME_DIR/cowboy` and persists
state to `$XDG_STATE_HOME/cowboy/daemon/state.json`.

## Worktree collisions

Starting `cowboy` in a worktree that already has a live session is refused by
default (the lease is held). Flags choose what happens instead:

- `--attach-if-active` — attach to the running session.
- `--read-only` — attach read-only (watch without driving).
- `--new-worktree` — create a fresh git worktree and run there.
- `--force-same-worktree` — take over a *stale* lease (never a live one).

## Attach, detach, replay

- `cowboy sessions` lists live/registered sessions and their status (including
  `blocked`, with a reason).
- Attaching streams the live journal; detaching leaves the session running.
- `cowboy logs` lists past sessions; `cowboy replay <id>` replays one from its
  recorded journal.

## Session state on disk

Each session writes to `.cowboy/sessions/<id>/` (gitignored): the transcript,
command logs, a diff, lifecycle/decision streams, published artifacts, and a
handoff. See [The agent & its tools](agent-and-tools.md).
