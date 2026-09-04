# Sessions & the daemon

A **session** is one agent run against a worktree. Sessions are coordinated by a
local daemon, **`cowboyd`**, which is started automatically when needed.

## How a session works

1. `cowboy` loads host-owned `security.yaml` (never visible to the agent).
2. It creates the session's namespaces — user, network, IPC, UTS — held open by a
   small holder process whose lifetime is tied to the worker's, so nothing outlives
   a crash.
3. Inside that network namespace it installs egress interception and starts the
   relay, **then** reports readiness — so there is no window in which a command
   could run unpoliced (see [Network egress](../security/network.md)).
4. The agent loop calls an OpenAI-compatible model with a tool surface (see
   [The agent & its tools](agent-and-tools.md)). The loop runs on the host.
5. Each shell command gets its **own** mount and PID namespace, built from the
   current grant set, with Landlock and seccomp applied before `exec`. Output is
   streamed to the UI and fed back to the model. The session is logged under
   `.cowboy/sessions/<id>/`.

Commands share the session's network namespace — so a dev server one command starts
is reachable from the next — but not its PID namespace, so ending a command reaps
exactly its own processes and one command cannot signal another's.

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

### It exits on its own

The daemon starts automatically and stops automatically. Once no session is live
and it is not serving the web UI, it lingers briefly and then exits, so closing
the last TUI leaves nothing running. A *detached* session counts as live — that is
the point of detaching — so this only fires when there is genuinely nothing left.

The linger exists so that starting a session does not race the daemon shutting
down. `COWBOY_DAEMON_LINGER` sets it in seconds (default `20`); `0` disables
idle exit entirely and the daemon stays up until asked to stop.

If you find a long-lived `cowboyd` with nothing running, the usual reason is
[`cowboy web`](web.md) being on — a served web UI is useful with no sessions, so it
holds the daemon up deliberately. "Serving" means the server is actually up, not
merely configured: a web UI whose bind failed does not keep the daemon alive.

Ending a session is unconditional rather than merely bounded. The worker unlinks
its socket and stops accepting *before* it tears anything down, so an ended session
cannot be attached to while it winds up, and both worker and daemon arm a watchdog
that hard-exits if teardown itself wedges.

### A vanished client ends the session

`End` reaches the worker over its socket, so it can be lost the way any message can:
a client killed outright, a terminal that closed, a client that raced its own
shutdown. The worker therefore does not wait for it. If the last client's connection
drops **without** a `Detach` first, the session ends after a few seconds' grace.

The distinction is the whole rule: `Detach` means "keep going, I'll be back" and
leaves the session running and reattachable, while a socket that simply closed means
nobody is driving. Nothing is lost either way — a turn already in flight runs to
completion first, and the transcript is on disk regardless.

Sessions the daemon drives itself (ranch workstreams) never have a client attach, so
this never applies to them.

## Upgrades

`cowboy` and `cowboyd` are version-locked. After you upgrade the binary, cowboy
keeps the two in sync automatically — you should never end up driving a new CLI
against a stale daemon:

- **Daemon roll.** The first `cowboy` command after an upgrade notices the
  running `cowboyd` is a different version and rolls it: the old daemon is asked
  to shut down (its workers keep running) and a matching one starts in its place.
  In-flight sessions survive — their workers re-register with the new daemon and
  stay attachable. Set `COWBOY_NO_DAEMON_AUTORESTART=1` to refuse instead (the
  command errors and tells you to restart `cowboyd` yourself).
There is no image or long-lived container to fall out of step: a sandbox is built
from the running binary's own plan every time a command starts, so an upgraded
binary is in force from its next command.

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

A session's worker socket accepts **multiple simultaneous clients**, so the same
session can be driven from more than one place at once — including a browser. See
[The web UI](web.md) for remote control (e.g. from your phone over Tailscale).

## Session state on disk

Each session writes to `.cowboy/sessions/<id>/` (gitignored): the transcript,
command logs, a diff, lifecycle/decision streams, published artifacts, and a
handoff. See [The agent & its tools](agent-and-tools.md).
