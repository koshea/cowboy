# The web UI (remote control)

`cowboy web` serves a browser UI for your sessions, so you can drive an agent from
any device on your network — e.g. keep coding from your phone — alongside (or
instead of) the terminal TUI. Because sessions live in the daemon and a session's
worker socket accepts **multiple simultaneous clients**, the web client is just
another attacher: it gets the same live event stream, journal replay, and approval
prompts as the TUI, and the TUI and a browser can drive the same session at once.

## Turning it on

It's a **setting**, not a command you babysit: enable it once and `cowboyd` serves
the UI itself, re-serving it on every daemon start.

```sh
cowboy web on                        # enable; cowboyd serves http://127.0.0.1:8787 and prints a tokened URL
cowboy web on --bind 100.x.y.z:8787  # bind your Tailscale IP to reach it from another device (prints a QR)
cowboy web status                    # is it enabled + serving? prints the URL (and QR for a remote bind)
cowboy web off                       # stop serving (the daemon keeps running)
```

The setting (enabled, bind, token, allow-lan) persists in `~/.config/cowboy/web.yaml`.

**It keeps `cowboyd` running.** Normally the daemon exits a few seconds after the last
session ends — there is nothing to supervise, and it restarts on the next command. A
served web UI is useful with no sessions at all (that is how you start one from your
phone), so while it is serving, the daemon stays up. If you find a `cowboyd` in `ps`
long after you closed everything, that is usually why; `cowboy web status` will say
`enabled · serving`, and `cowboy web off` lets it exit when idle again.

If the bind fails — a Tailscale IP that is not up when the daemon starts, or a port
already taken — the daemon says so in its log, reports `enabled · not serving`, and
exits when idle like any other idle daemon rather than lingering for a UI that never
came up. Fix the bind and run `cowboy web on` again.

## What you can do

Open the URL and you get a list of your sessions — newest first, with the project
and start time, refreshed every few seconds. Tap one to:

- watch the transcript stream live — model output renders as **markdown as it
  arrives**, alongside command output (progress lines update in place), diffs, plan
  steps, and a header with status, tokens, a context meter (amber from 70%, red from
  90%, naming the biggest consumer), cost and the diffstat;
- **send messages**. While the agent is working a message *steers* the current turn;
  **Later** queues it to run afterwards instead, and the `⏭ N queued` bar lists and
  clears the queue. On a phone, Enter is a newline and the Send button sends;
- use the same **slash commands as the TUI** — `/plan`, `/go`, `/after`,
  `/queue clear`, `/model`, `/stop`, `/accept`, `/ranch`, skills (`/<name> args`),
  `/diff`, `/mcp`, `/boundary`, `/crew`, `/context`, `/jobs`, `/copy`, `/fold`,
  `/clear`; `/help` lists them. They're expanded by the session's worker, so they
  behave identically from either client;
- **answer questions** and **approve or deny** access prompts with the TUI's choices
  — once, session, project or global for a network request; allow or deny for a
  credential, which is never remembered. Several outstanding prompts are shown one
  at a time, oldest first;
- **interrupt** the current turn (■), **stop the subagents** it dispatched, or
  **end** the session;
- start a fresh session from the same **openers** the TUI offers;
- **watch a subagent** — when the agent fans work out to a [crew](crew.md), the
  subagents appear as chips above the transcript; tap one to open its live output
  read-only (and tap back to return). A chip marked `?` is a worker waiting on the
  foreman (a turn grant or a question); a pending one isn't tappable until it starts.

Network activity and the session's background processes are under collapsible
panels below the header.

The view **sticks to the bottom** as new content streams in (scroll up to read
back; it re-follows when you return to the bottom). If the connection drops — a
phone sleeping, a network switch — it **reconnects automatically** and resumes the
journal where it left off; while it's down, anything you send is refused with a
notice rather than held and delivered later out of context. A finished session opens
**read-only**, replaying its recorded transcript. A page older than the session's
worker skips events it doesn't understand (and says so) instead of stalling.

## Access & exposure

The web server grants full control of your sessions, so it's locked down:

- **Token.** Every request needs the bearer token — minted on first `on`, stored
  `0600` in `web.yaml`, and embedded in the printed URL. On a remote bind, `on`
  and `status` also print a scannable **QR code** of the tokened URL.
- **Bind.** Loopback by default. A **Tailscale** address (`100.64.0.0/10`) is
  allowed because Tailscale encrypts and authenticates the transport
  device-to-device — the recommended way to reach it remotely. Any other
  non-loopback bind (a LAN IP, `0.0.0.0`) is **refused** unless you pass `--lan`,
  since the token would otherwise travel in cleartext. For anything else, keep the
  loopback bind and tunnel in (`ssh -L 8787:127.0.0.1:8787 …`).
- **Model output cannot reach the network.** The transcript renders the agent's
  markdown, and an `<img>` would be fetched by the *browser* the moment it appeared
  — egress that goes around the sandbox policy entirely, since the request does not
  come from the sandbox. Images are therefore rendered as click-through links
  instead, and a `Content-Security-Policy` with `img-src 'self' data:` blocks the
  fetch even if one ever slips past that.
- **Response headers.** `Referrer-Policy: no-referrer` (the token is in the URL, so
  it is not left resting on a browser default), `Cache-Control: no-store`,
  `nosniff`, `X-Frame-Options: DENY`, and a CSP whose `script-src` carries no
  `'unsafe-inline'` — the bundle's own loader is allowed by SHA-256 hash, computed
  from the embedded shell at startup. That way an escaping bug in the markdown
  renderer cannot become script execution with the token behind it.

This mirrors the rest of cowboy's model: the host owns the boundary, access is
token-gated and fails closed, and nothing binds beyond localhost by default.

## Building from source

The UI is a [Yew](https://yew.rs) WASM app built with [trunk](https://trunkrs.dev)
and embedded into the `cowboy` binary. A plain `cargo build` without trunk yields a
working server with a placeholder page; to embed the real UI, run
`trunk build --release` in `crates/cowboy-web-ui` before building (CI does this for
release artifacts).
