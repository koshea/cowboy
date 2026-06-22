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

## Upgrades

`cowboy` and `cowboyd` are version-locked, as are the agent/gateway container
images (each is pinned to the binary version). After you upgrade the binary,
cowboy keeps the two in sync automatically — you should never end up driving a
new CLI against a stale daemon or a stale container:

- **Daemon roll.** The first `cowboy` command after an upgrade notices the
  running `cowboyd` is a different version and rolls it: the old daemon is asked
  to shut down (its workers keep running) and a matching one starts in its place.
  In-flight sessions survive — their workers re-register with the new daemon and
  stay attachable. Set `COWBOY_NO_DAEMON_AUTORESTART=1` to refuse instead (the
  command errors and tells you to restart `cowboyd` yourself).
- **Container recreate.** A session's agent/gateway container is stamped with the
  version that created it. If a new binary finds a container left by an older
  version, it removes and recreates it from the current image rather than
  reusing it — so you never silently run a stale (possibly outdated) sandbox or
  gateway. A live, same-version session keeps its container untouched.

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

## Remote control from a browser (`cowboy web`)

Because sessions live in the daemon and the worker socket accepts **multiple
simultaneous clients**, you can drive a session from a browser — e.g. keep coding
from your phone — while your terminal TUI stays attached to the same session.

It's a **setting**, not a command you babysit: turn it on once and `cowboyd`
serves the web UI itself (and re-serves it whenever the daemon starts).

```sh
cowboy web on                       # enable; cowboyd serves http://127.0.0.1:8787, prints a tokened URL
cowboy web on --bind 100.x.y.z:8787 # bind your Tailscale IP to reach it from another device
cowboy web status                   # is it enabled + serving? prints the URL (and a QR for a remote bind)
cowboy web off                      # stop serving (the daemon keeps running)
```

The daemon bridges each browser WebSocket to a session's worker socket — the web
client is just another attacher, so it gets the same live stream, journal replay,
and approval prompts as the TUI. The page lists your sessions; open one to see its
transcript, send messages, answer questions, approve network requests, and
interrupt turns. If the connection drops (a phone sleeping, a network switch) the
page **reconnects automatically** and resumes the journal where it left off. With
a remote bind, `cowboy web on`/`status` also print a **QR code** of the tokened
URL, so you can point your phone's camera at the terminal to open it.

**Access & exposure.** The setting lives in `~/.config/cowboy/web.yaml` (`0600` —
it holds the access token, minted on first `on`). Every request needs that token
(embedded in the `open:` URL). It **binds loopback by default**; for remote access
it allows a **Tailscale** address (`100.64.0.0/10`), which encrypts and
authenticates the transport device-to-device. Any other non-loopback bind (a plain
LAN IP, `0.0.0.0`) is **refused** unless you pass `--lan`, because the token would
otherwise travel in cleartext — prefer Tailscale, or an SSH tunnel
(`ssh -L 8787:127.0.0.1:8787 …`) to the default loopback bind.

> The web UI is a WASM bundle built with [trunk](https://trunkrs.dev) and embedded
> into the `cowboy` binary. Building from source without trunk yields a working
> server with a placeholder page; run `trunk build --release` in
> `crates/cowboy-web-ui` before `cargo build` to embed the real UI (CI does this).

## Session state on disk

Each session writes to `.cowboy/sessions/<id>/` (gitignored): the transcript,
command logs, a diff, lifecycle/decision streams, published artifacts, and a
handoff. See [The agent & its tools](agent-and-tools.md).
