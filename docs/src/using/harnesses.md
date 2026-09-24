# Harnesses: delegating to other agent CLIs

A **harness** is another vendor's coding agent — today **grok** (Grok Build) — that
cowboy's crew can hand work to, in addition to the models cowboy's own loop runs.
The point is to use your **subscription plans**, which only work inside the vendor's
own CLI: route a kind of work to it (`exploration: grok`), or ask for it by name
("have grok review this PR").

## Configure

Harnesses live in the **user-level** `~/.config/cowboy/harnesses.yaml` — one entry
per CLI. There is no project-level file: a definition decides what a sandboxed process
may see (your vendor login) and reach (the vendor's API), so a cloned repo must not be
able to ship one.

```yaml
harnesses:
  grok:
    kind: grok
    model: grok-4.7          # optional; the CLI's default otherwise
    auth: auth_file          # default; or full_home (see below)
    stall_minutes: 10        # quiet this long → the foreman is told (never killed)
    allow_hosts: []          # extra hosts, on top of grok's own
    extra_args: []           # appended to the command line
```

`cowboy harnesses` shows each one and whether it is ready (binary found, logged in);
`cowboy doctor` checks the same. Log in with the vendor's own CLI on the host first
(`grok login`).

## Use it

- **Routing.** In `crew.yaml`, a category slot may name a harness instead of a model:
  `exploration: grok`. A name defined in both `models.yaml` and `harnesses.yaml` is a
  configuration error (`cowboy crew validate` reports it).
- **By name.** Ask the agent to "have grok …" and it passes `harness: grok` to the
  `subagent` tool. The foreman only picks a harness itself through the roster.

A harness job works in the **same workspace** as everything else, like any subagent,
and shows up the same way in the TUI and the web UI — a chip you can open to watch it
live (its commands, edits, output and cost). When it finishes, the foreman gets the
harness's answer **plus a change summary cowboy measured itself** (`git diff --stat`
of the workspace before and after), so what it touched is not just what it claimed.

There are **no turn or time limits**: a harness runs until it finishes or you stop it
(the stop-subagents control). If it goes quiet for `stall_minutes`, the foreman gets a
job update saying so and can stop it; nothing is killed automatically. If the harness
cannot run — not installed, logged out, a subscription limit — the job fails with the
reason; it does not fall back to a model.

## How it is confined

A harness always runs **inside cowboy's sandbox**, never on the host. It is launched
with its own approval prompts and sandbox turned off (`--always-approve`), which is
safe only because cowboy's kernel boundary is the one that holds.

- **Files.** It sees the project (read-write, like any subagent), its own binary, and a
  **private home** for this job. Your real `~/.grok` is never mounted — it holds the
  grok binary, hooks and MCP server commands that your host grok later runs
  unconfined, so a writable mount would let the harness plant code outside the sandbox.
  The vendor homes (`~/.grok`, `~/.claude`, `~/.codex`, `~/.gemini`) are also on the
  denylist, so a runtime grant cannot expose them either.
- **Your login.** With `auth: auth_file` (the default) the private home gets a copy of
  the login file alone. With `auth: full_home` it also gets your grok configuration and
  credentials (config, MCP credentials, skills, plugins) — not binaries, logs or
  session history. Either way, whatever it is given is readable by the harness's own
  model; treat it as disclosed to it. If the harness refreshes its login, the new
  token is written back to your real login file (unless your host grok refreshed it
  meanwhile, in which case yours wins).
- **Network.** The harness's own API and login hosts are allowed for its job. Any other
  destination is put to **you**: it appears as a normal network approval in the TUI or
  web UI, labelled with the job that asked, and fails closed if nobody answers.
- **Talking to the foreman.** The harness gets a small `cowboy` MCP server with two
  tools, `ask_foreman` (a question the foreman answers, as it does for its own
  subagents) and `report_progress` (a job update). Nothing else crosses: the job's
  control channel itself is never exposed to the harness.
