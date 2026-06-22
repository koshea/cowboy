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

## What you can do

Open the URL and you get a list of your sessions; tap one to:

- watch the transcript stream live — model output renders as **markdown as it
  arrives**, alongside command output, diffs, plan steps, and a token/cost header;
- **send messages** and answer the agent's questions;
- **approve or deny** the agent's network-access prompts;
- **interrupt** the current turn.

The view **sticks to the bottom** as new content streams in (scroll up to read
back; it re-follows when you return to the bottom). If the connection drops — a
phone sleeping, a network switch — it **reconnects automatically** and resumes the
journal where it left off. A finished session opens **read-only**, replaying its
recorded transcript.

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

This mirrors the rest of cowboy's model: the host owns the boundary, access is
token-gated and fails closed, and nothing binds beyond localhost by default.

## Building from source

The UI is a [Yew](https://yew.rs) WASM app built with [trunk](https://trunkrs.dev)
and embedded into the `cowboy` binary. A plain `cargo build` without trunk yields a
working server with a placeholder page; to embed the real UI, run
`trunk build --release` in `crates/cowboy-web-ui` before building (CI does this for
release artifacts).
