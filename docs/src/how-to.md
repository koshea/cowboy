# How-to guides

Task-oriented recipes for the things you do with Cowboy every day — from
interrupting and redirecting a run to coordinating a multi-workstream ranch plan.
Each one is the shortest path that works.

For the full key/command reference inside the TUI, type `/help`.

---

## Start a task

```bash
cowboy "add a --json flag to the export command"   # start with a task
cowboy                                              # or start empty and type
```

Cowboy runs the agent in a Docker sandbox, opens the TUI, and streams the work.
Send a follow-up any time the agent is idle: **Enter** sends, **Shift+Enter**
inserts a newline. The first run builds the agent image (a few minutes, shown as
a step) and, if the repo uses [mise](https://mise.jdx.dev), installs the toolchain
once into a persistent cache.

## Interrupt and redirect mid-run

Press **Ctrl-C** while the agent is working to open the pause menu (each option is
labeled on screen):

- **r** resume — keep going
- **i** instruct — stop this turn and give a new direction (history is kept; your
  next message starts a fresh turn)
- **k** kill — stop just the running command/turn; the session stays put
- **d** detach — leave it running in the background and exit
- **e** end — finish the session

The common case is **i**: you see the agent heading the wrong way, hit Ctrl-C,
press `i`, and type the correction. **Esc** resumes.

## Exit, detach, and come back later

- **Detach** (Ctrl-C → `d`, or `/detach`) leaves the session running in the
  background under the daemon. Re-attach later.
- **End** (Ctrl-C → `e`, or `/quit`) finishes the session.

Coming back:

```bash
cowboy --continue               # resume the most recent session in THIS worktree
cowboy sessions                 # list sessions: id, status, branch, task
cowboy attach <session-id>      # re-join a live session (or replay if finished)
cowboy --resume <session-id>    # resume a specific session, keeping history
```

`--continue` is the quick path in the same worktree; use `cowboy sessions` to find
a session id from elsewhere. A finished session re-opens read-only;
`cowboy replay <id>` does the same from the on-disk journal.

## See what changed and commit it

Inside the TUI, `/diff` shows the working-tree diff, and file edits render as
inline colored diffs as the agent makes them. From the shell:

```bash
cowboy patch show     # print the working-tree diff
cowboy patch save     # write it to .cowboy/diff.patch
cowboy patch check    # dry-run: does a patch apply cleanly?
cowboy patch apply    # apply a patch from stdin
cowboy patch revert   # discard uncommitted changes (asks to confirm)
```

The workspace is bind-mounted, so edits land in your real working tree — commit
with normal `git` (`git add -p`, `git commit`), or just ask the agent to commit.

## Switch model · solo vs. crew

- `/model` — show the current model and the available list.
- `/model <name>` — switch for the next turn (this session only).
- `/models` — open the picker: type to filter, **Enter** to select, **Tab** to
  toggle **Solo ⇄ Crew** for the highlighted model, **Esc** to cancel.
- `cowboy models use <name> [--global]` — make a model the persistent default
  (a `/model` switch only lasts the session).

**Solo** = one model does everything. **Crew** = your model is the *foreman* and
delegates sub-tasks to specialists; see `/crew` for the routing table.

## Approve (or deny) a network request

Cowboy denies network by default and asks when the agent reaches for something
new. The prompt explains each scope:

- **o** once — just this request
- **s** session — every request to this host until the session ends
- **p** project — always allow here (saved for this repo)
- **g** global — always allow everywhere
- **d** deny (Esc also denies)

Project/global approvals persist host-side and apply silently next time. The
prompt times out to *deny* after two minutes (fail-closed). To skip prompts for
hosts you already trust, pre-allow them in `.cowboy/security.yaml`. A blocked
request shows a `🛡 blocked …` note explaining what to allow.

## Run a long task and walk away

Start the task, press **Ctrl-C → d** (detach), and close the terminal. The daemon
keeps the session running, and the worker survives a daemon restart. Check back
with `cowboy sessions` and `cowboy attach <id>`. Everything is journaled to
`.cowboy/sessions/<id>/`, so even a crashed session can be replayed; `cowboy
session cleanup` reaps stale records.

## Review a finished session

```bash
cowboy review [session-id]      # handoff + artifacts + decisions + diffstat
cowboy handoff [session-id]     # just the handoff summary
cowboy artifact list [session]  # published artifacts (contracts, reviews, …)
cowboy decisions list [session] # recorded decisions
```

`cowboy review` records itself as a Review artifact you can annotate; pair it with
`cowboy patch show` to see the full diff.

## Work on several things at once (worktrees)

```bash
cowboy --new-worktree "refactor the parser"   # run in a fresh git worktree/branch
cowboy worktree list                          # worktrees + which session holds each
cowboy worktree diff [branch]                 # what changed vs the fork point
cowboy worktree status [branch]               # mergeability + file list
```

Each worktree is an isolated branch + session, so parallel tasks never collide.
Merging a finished branch back to your main line is normal `git` — Cowboy never
auto-merges.

## Plan first, then execute (plan mode)

When you want to see the approach *before* the agent touches your code:

```text
/plan add request caching to the API client
# the agent researches read-only and proposes a plan — file edits are BLOCKED
/go                       # approve → the agent implements the plan
/go also add a test       # approve with an extra instruction
```

While plan mode is on, Cowboy **refuses `edit`/`write` host-side** — not just by
prompting the model. The agent reads/greps, fills the plan pane, and presents a
plan; if it tries to edit, it's told to wait. The status bar shows `🧭 plan mode`
the whole time so you always know edits are gated. `/go` lifts the gate and the
same conversation continues into execution; keep refining with more messages
before you approve.

**Too big for one session?** Type `/ranch` to promote the discussion into a
multi-workstream ranch plan — the agent reuses what you've discussed, decomposes
it into workstreams, and drafts a `ranch.yaml` (below).

## Run a multi-workstream ranch plan

A *ranch* coordinates several dependent workstreams. Each workstream is its own
Cowboy session in its own worktree — and each is **interactive**: you pick it up,
drive it, and sign off when you're happy. They're large chunks of work, so they're
meant to be steered, not fired-and-forgotten.

Let the agent decompose the goal:

```bash
cowboy ranch plan "add Stripe billing + invoicing"
```

It researches your codebase read-only, proposes workstreams with dependencies,
and writes a draft `ranch.yaml` (it edits nothing). Review, tweak, then start. Or
build it by hand:

```bash
cowboy ranch create "Billing" --goal "Stripe + invoicing"
cowboy ranch add bill schema --goal "billing tables + migrations"
cowboy ranch add bill api    --goal "billing API" --depends-on schema --expects api-contract.md
cowboy ranch add bill ui     --goal "billing UI"  --depends-on api
cowboy ranch start bill          # launch ready workstreams (deps satisfied)
cowboy ranch status bill         # dependency tree + readiness
cowboy ranch attach bill schema  # pick up a workstream and drive it
cowboy ranch watch bill          # live dashboard (s advances, q quits)
```

`cowboy ranch add` and `cowboy ranch plan` both validate the dependency graph — a
typo'd `--depends-on` or a cycle is rejected up front, so a plan never silently
deadlocks at `start`. `cowboy ranch status` lays the workstreams out as a
**dependency tree** in execution order, glyphed by status
(`✓ done · ⟳ running · ◷ ready · ⊘ blocked · ⏸ waiting`) with what each waits on.

### How a workstream runs

`cowboy ranch start` launches every ready workstream as a **detached session**,
each seeded with its goal and the artifacts its dependencies published. Each
session **runs an initial attempt on its own**, then idles — waiting for you.

Pick one up with `cowboy ranch attach <ranch> <ws>`, review the first attempt,
and refine it like any normal session (send messages, `/diff`, `/plan`, switch
models). When you're happy, sign off **from inside the session**:

```text
/accept                  # sign off → completes the workstream, advances the plan
/accept ui matches the mockups
```

`/accept` marks the workstream complete, promotes its artifacts into the ranch's
committed store, launches any newly-unblocked workstreams, and ends the session.
A workstream is **never** auto-completed — sign-off is always your explicit call.
(If you'd rather sign off from the shell, `cowboy ranch accept <ranch> <ws>` does
the same thing.)

Finished branches are left for you to merge. Scope changes go through
`cowboy ranch propose` → `approve`/`reject`; the agent never edits the plan itself.

## Manage credentials the agent may use

```bash
cowboy secrets add gh --repo        # print a grant snippet for a preset
cowboy secrets list                 # show grants + whether the host source exists
```

Presets cover common tools (`gh`, `aws`, `gcloud`, `kubectl`, `git`, `ssh`) — see
`cowboy secrets add --help`. Grants live in `.cowboy/security.yaml` (or the home
overlay); the credential is mounted read-only and its *value* never lands in
config. See [Configuration](getting-started/configuration.md).

## Customize the agent image (per repo)

When a repo needs tools the base image lacks (system libraries, extra languages,
build headers like `libpq-dev` for the `pg` gem), commit a **`.cowboy/Dockerfile`**
that extends the base image:

```dockerfile
# .cowboy/Dockerfile
FROM ghcr.io/koshea/cowboy/agent:0.1.0
RUN apt-get update && apt-get install -y --no-install-recommends libpq-dev \
    && rm -rf /var/lib/apt/lists/*
```

That's it — commit it and **every contributor's next session automatically builds
and uses it**; no flags or extra config. Cowboy builds the base first, then your
image, tagged by the file's content so **editing the Dockerfile rebuilds it** (end
the session / `cowboy down` first so the container is recreated on the new image).

Notes:
- Always `FROM` the cowboy base image (`ghcr.io/koshea/cowboy/agent:<version>`) so
  you inherit the toolchains, `mise`, the in-container `cowboy` helper, and the
  security wiring. Match the tag to your installed `cowboy --version` (contributors
  building from source get that tag built locally); cowboy ensures the base exists
  before building your image.
- The build context is the repo root, so you can `COPY` project files; add a
  `.dockerignore` if the repo is large.
- Building runs the Dockerfile on each contributor's machine — the same trust as
  the repo's other build scripts (it changes only what's *inside* the sandbox; the
  network/credential boundary is unchanged).
- For a fully custom or registry image instead, set `container.image` (and
  optionally `container.dockerfile`) in `.cowboy/security.yaml`.
- If a build is killed with `exit 137` (out of memory), raise `cpus`/`memory` in
  `.cowboy/security.yaml` (build parallelism follows `cpus`) — see
  [Configuration](getting-started/configuration.md).

## Use a skill

Skills are reusable, named prompts. Type `/` in the TUI to autocomplete the
available skills and commands, or `/skills` to list them; run one with
`/<skill> [args]`, e.g. `/github:review-pr 162`.

## Connect an MCP server

[MCP](https://modelcontextprotocol.io) servers give the agent external tools —
issue trackers, docs search, internal APIs. They're **trusted integrations you
configure**: they run on the host (outside the agent's container), the agent can
*call* them but never add or edit them, and their config + credentials stay
host-owned in `~/.config/cowboy/mcp.yaml`.

```bash
# A local stdio server (a subprocess on your machine):
cowboy mcp add filesystem --transport stdio \
  --command npx --arg -y --arg @modelcontextprotocol/server-filesystem --arg /workspace \
  --description "read/write files under /workspace"

# A remote HTTP server (use ${VAR} for secrets — never paste the token itself):
cowboy mcp add linear --transport http --url https://mcp.linear.app/sse \
  --header "Authorization=Bearer \${LINEAR_TOKEN}" --description "issue tracking"

cowboy mcp list                 # configured servers + status
cowboy mcp test linear          # connect and list the server's tools
cowboy mcp disable linear       # keep it configured but off
cowboy mcp remove linear        # delete it
```

In a session, the agent is told **which servers are connected and what each is
for** (one line each) — it doesn't carry every tool's schema in every request.
When it needs a server it uses the built-in `mcp` tool to **discover** that
server's tools (full schemas, on demand) and **call** them. Type `/mcp` in the
TUI to see the connected servers yourself.

Use `--tool <name>` (repeatable) on `add` to expose only specific tools from a
chatty server. `${VAR}` in `--env`/`--header` values is expanded from your host
environment at connect time, so secrets never live in the config file.

### Project servers from `.mcp.json`

A repo can ship a `.mcp.json` at its root (the same format other MCP clients use)
declaring the servers that project expects:

```json
{
  "mcpServers": {
    "docs": { "command": "npx", "args": ["-y", "@acme/docs-mcp"] },
    "api":  { "type": "http", "url": "https://mcp.internal/acme" }
  }
}
```

Because a `.mcp.json` server runs a command (or reaches an endpoint) **on your
host**, Cowboy **ignores it until you trust it** — opening a repo never silently
runs its servers:

```bash
cowboy mcp trust      # review this repo's .mcp.json servers, then approve them
cowboy mcp list       # shows host servers + the repo's servers and their trust state
cowboy mcp untrust    # revoke
```

Trust is recorded host-side (never in the repo) and pinned to the exact server set
you approved: if `.mcp.json` later changes, it goes **stale** and you must
`cowboy mcp trust` again. Your host `mcp.yaml` always wins over a repo server of the
same name. A session in a repo with an untrusted `.mcp.json` shows a one-line notice
pointing you to `cowboy mcp trust`.
