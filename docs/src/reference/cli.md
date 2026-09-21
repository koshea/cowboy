# CLI reference

<!-- GENERATED from the clap command tree by `cargo test -p cowboy-cli --test cli_docs`.
     Do not edit by hand. Regenerate with:
     COWBOY_REGEN_DOCS=1 cargo test -p cowboy-cli --test cli_docs -->

An opinionated local coding agent that runs wild inside a corral you own.

## `cowboy` (global options)

| Arg | Description |
|-----|-------------|
| `<TASK>` | Optional one-shot task. With no subcommand, `cowboy 'fix the tests'` starts a session with the task prefilled |
| `-v, --verbose` | Enable debug logging (or set COWBOY_LOG=...) |
| `-y, --yes` | Answer every confirmation with yes (or set COWBOY_ASSUME_YES=1) |
| `--attach-if-active` | On a same-worktree collision, attach to the active session instead of prompting |
| `--read-only` | On a same-worktree collision, attach read-only (watch without driving) |
| `--new-worktree` | On a same-worktree collision, create a new git worktree and run there |
| `--force-same-worktree` | Take over a *stale* lease on this worktree (never a live one) |
| `--continue` | Continue the most recent session in this worktree, keeping its history |
| `--resume` | Resume a specific session by id, keeping its conversation history |

```text
Getting started:
  cowboy init                     # set up .cowboy/ in this repo
  cowboy models setup             # configure a provider + model (once per machine)
  cowboy doctor                   # check the host can sandbox and the config is sane

Everyday use:
  cowboy                          # open the TUI and pick a task
  cowboy "fix the failing tests"  # start with the task prefilled
  cowboy --continue               # resume the most recent session in this worktree
  cowboy sessions                 # list sessions, then: cowboy attach <id>
  cowboy down                     # end this project's sessions

Type /help inside the TUI for keys and slash commands.
```


## `cowboy agents`

List or show agent definitions (specialist personas under .claude/agents/)


### `cowboy agents list`

List available agent definitions (name + description + model)


### `cowboy agents show`

Print an agent's definition (its system prompt / review approach)

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


## `cowboy artifact`

Inspect or publish session artifacts (contracts, summaries, handoffs, …)


### `cowboy artifact add`

Publish a file as a session artifact

| Arg | Description |
|-----|-------------|
| `<PATH>` | Path to the file to publish |
| `--kind` | What sort of artifact this is (defaults to `notes`) |
| `--title` | Friendly title (defaults to the file name) |
| `--summary` | One-line summary |
| `--session` | Session to publish into (defaults to the most recent in this worktree) |


### `cowboy artifact list`

List artifacts for a session (defaults to the most recent)

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |


### `cowboy artifact show`

Print an artifact's body by id

| Arg | Description |
|-----|-------------|
| `<ID>` | The artifact id, as shown by `cowboy artifact list` |
| `--session` | Session to read from (defaults to the most recent in this worktree) |


## `cowboy attach`

Attach the TUI to a running session (by id, or a worker socket path)

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |


## `cowboy completions`

Print a shell completion script

| Arg | Description |
|-----|-------------|
| `<SHELL>` |  |

```text
Examples:
  cowboy completions zsh  > "${fpath[1]}/_cowboy"
  cowboy completions bash > ~/.local/share/bash-completion/completions/cowboy
  cowboy completions fish > ~/.config/fish/completions/cowboy.fish
```


## `cowboy crew`

Manage the Crew Roster (route delegated work to models by category/effort)


### `cowboy crew init`

Write a default crew roster (tiers derived from your models' prices)

| Arg | Description |
|-----|-------------|
| `--force` | Overwrite an existing crew.yaml (asks first) |


### `cowboy crew list`

Show the routing matrix (category × effort → model)


### `cowboy crew recommend`

Suggest roster changes from recorded outcomes (recommend-only; never edits)


### `cowboy crew show`

Print the full crew.yaml (roster + delegation rules)


### `cowboy crew usage`

Show recorded delegation usage per model (tasks, success %, avg duration)


### `cowboy crew validate`

Check the roster (models exist, `general` defined, etc.)


## `cowboy decisions`

List or show decisions recorded in a session


### `cowboy decisions list`

List recorded decisions (defaults to the most recent session)

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |


### `cowboy decisions show`

Show one decision by id

| Arg | Description |
|-----|-------------|
| `<ID>` | The decision id, as shown by `cowboy decisions list` |
| `--session` | Session to read from (defaults to the most recent in this worktree) |


## `cowboy doctor`

Check kernel prerequisites, model config, and the egress gateway


## `cowboy down`

End this project's running sessions and release their sandboxes

| Arg | Description |
|-----|-------------|
| `--all` | End sessions for EVERY project, not just this one (asks first) |


## `cowboy grant`

Let the sandbox see a host path outside this project

| Arg | Description |
|-----|-------------|
| `<PATH>` | The host path to grant. Omit with `--list` |
| `--ro` | Grant read-only access. The default is read-write, since a path you ask for by hand is usually one you intend to work in |
| `--global` | Remember for every project on this machine, not just this one |
| `--remove` | Forget a previously granted path |
| `--list` | Show the saved grants for this project |

```text
Examples:
  cowboy grant ~/src/shared-lib           # read-write, this project only
  cowboy grant --ro /opt/reference-data   # read-only
  cowboy grant --global ~/src/shared-lib  # every project on this machine
  cowboy grant --list                     # what is granted here
  cowboy grant --remove ~/src/shared-lib  # take it back
```


## `cowboy handoff`

Print a session's handoff summary (defaults to the most recent)

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |


## `cowboy inbox`

Read a session's message inbox (defaults to the most recent). Reading drains the inbox unless --peek is given

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |
| `--peek` | Show the messages without consuming them |


## `cowboy init`

Create initial project config files under `.cowboy/`

| Arg | Description |
|-----|-------------|
| `--force` | Overwrite existing config files if present (asks first) |
| `--git` | Also run `git init` if the project is not already a git repository |


## `cowboy logs`

List session logs


## `cowboy mcp`

Configure MCP servers the agent can discover and call (host-owned)


### `cowboy mcp add`

Add or replace an MCP server in ~/.config/cowboy/mcp.yaml

| Arg | Description |
|-----|-------------|
| `<NAME>` | Local name for the server (e.g. `linear`, `filesystem`) |
| `--transport` | Transport: `stdio` (local subprocess) or `http` (remote endpoint) |
| `--description` | One-line description shown to the agent (e.g. "issue tracking") |
| `--command` | stdio: the command to run (e.g. `npx`) |
| `--arg` | stdio: an argument to the command (repeatable, in order). Leading-dash values are fine (e.g. `--arg -y`) |
| `--env` | stdio: an environment variable, `KEY=VALUE` (repeatable). Use `${VAR}` in VALUE to reference host env; never inline secret literals |
| `--url` | http: the server URL |
| `--header` | http: a request header, `KEY=VALUE` (repeatable). Use `${VAR}` in VALUE |
| `--tool` | Tool names to expose (repeatable), fail-closed: omit to expose NONE, or pass `--tool '*'` to expose all of the server's tools |

```text
Examples:
  cowboy mcp add filesystem --transport stdio --command npx --arg -y --arg @modelcontextprotocol/server-filesystem --arg /workspace --description "files under /workspace" --tool "*"
  cowboy mcp add docs --transport http --url https://mcp.example.com/sse --header "Authorization=Bearer ${TOKEN}" --tool search

--tool is fail-closed: with none given the server is configured but exposes nothing.
Pass --tool '*' to expose everything, or name each tool. Check the result with
`cowboy mcp test <name>`.
```


### `cowboy mcp disable`

Disable a server (kept in config, not connected)

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy mcp enable`

Enable a configured server

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy mcp list`

List configured MCP servers (name, transport, enabled)


### `cowboy mcp remove`

Remove an MCP server

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy mcp show`

Show one server's full configuration

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy mcp test`

Connect to a server and list its tools (a connectivity check)

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy mcp trust`

Trust this repo's `.mcp.json` servers (review + approve them; required before the agent can use repo-defined servers). Re-run if the file changes


### `cowboy mcp untrust`

Revoke trust for this repo's `.mcp.json` servers


## `cowboy memory`

Inspect the agent's saved memory (project + global)


### `cowboy memory delete`

Delete a memory by name (shows it, then asks)

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy memory list`

List saved memories (project + global) for the current worktree


### `cowboy memory show`

Print a memory's full body by name

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


## `cowboy message`

Send a structured message to a session inbox (daemon-mediated bus)

| Arg | Description |
|-----|-------------|
| `<MESSAGE>` | The message text |
| `--to` | Target session id |
| `--all` | Broadcast to all other sessions instead of one |

```text
Examples:
  cowboy message "the API contract changed" --to 1788401869978-1
  cowboy message "pausing for a release" --all
```


## `cowboy models`

Configure model providers (home-owned) and models

```text
Examples:
  cowboy models setup                  # the guided path: provider, key, then a model
  cowboy models list                   # what is configured, and the effective default
  cowboy models available              # what your endpoint actually offers
  cowboy models use claude-sonnet-4-6  # set the project default

Credentials live only in ~/.config/cowboy/providers.yaml (mode 0600) and are read
host-side. They are never written into a project or bound into the sandbox.
```


### `cowboy models add`

Register a model by its provider id, prefilled from shipped defaults

| Arg | Description |
|-----|-------------|
| `<ID>` | The provider-side model id, e.g. `cerebras/zai-glm-4.7` |
| `--name` | Friendly name (config key). Defaults to the recommended name |
| `--provider` | Provider to use (defaults to the only configured one) |
| `--temp` | Sampling temperature (provider default if omitted) |
| `--context` | Context window in tokens, used to size the /context gauge and to decide when to compact |
| `--max-output` | Cap on tokens generated per response |
| `--reasoning` | Reasoning effort to request. `none` sends no hint at all |
| `--default` | Make this the default model |

```text
Examples:
  cowboy models add anthropic/claude-sonnet-4-6
  cowboy models add cerebras/zai-glm-4.7 --name fast --default
  cowboy models add openai/gpt-5 --reasoning high --max-output 32000

Shipped defaults fill in temperature, context window and pricing for known ids;
`cowboy models available` lists what your endpoint actually offers.
```


### `cowboy models available`

List models offered by the configured provider endpoints (chat models only unless `--all`), with recommended names and config status

| Arg | Description |
|-----|-------------|
| `--all` | Include non-chat models (image/audio/embedding/etc) |


### `cowboy models list`

List configured providers and models, and the effective default


### `cowboy models setup`

Interactively add a provider (endpoint + key, saved to your home dir) and a model that uses it


### `cowboy models use`

Set the default model. Writes to the project unless `--global`

| Arg | Description |
|-----|-------------|
| `<NAME>` | The model name to make default |
| `-g, --global` | Set the user-level (home) default instead of the project default |


## `cowboy patch`

Patch helper (wraps git inside the sandbox)

```text
Examples:
  cowboy patch show   # the working-tree diff
  cowboy patch save   # write it to .cowboy/diff.patch
  cowboy patch revert # discard uncommitted tracked changes (asks first)

The workspace is bind-mounted, so the agent's edits are already in your real working
tree — commit them with plain git.
```


### `cowboy patch apply`

Apply a patch read from stdin


### `cowboy patch check`

Validate that a patch from stdin applies cleanly


### `cowboy patch revert`

Revert uncommitted changes (asks for confirmation)


### `cowboy patch save`

Save the current git diff to `.cowboy/diff.patch`


### `cowboy patch show`

Display the current git diff


## `cowboy proc`

Inspect the session's long-running processes (the agent starts them)


### `cowboy proc list`

List configured processes and their status


### `cowboy proc logs`

Stream logs for a process

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy proc restart`

Restart a process by name

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy proc start`

Explain why a process cannot be started from here (they are session-owned)

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy proc stop`

Stop a process by name (only reaches a stale one; session processes end with the session)

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


## `cowboy ranch`

Create or inspect Ranch Plans (multi-workstream tasks)

```text
A ranch splits one large task into dependency-aware workstreams, each a normal session in
its own worktree and branch. The usual arc:

  cowboy ranch plan "migrate to the new auth service"  # an agent proposes the workstreams
  cowboy ranch status my-ranch                         # review the plan it drafted
  cowboy ranch start my-ranch                          # launch whatever is ready
  cowboy ranch watch my-ranch                          # live dashboard
  cowboy ranch accept my-ranch api-layer               # sign off a gated workstream

`plan` reads the codebase and starts nothing, so the plan is yours to edit first.
`ranch draft <spec>` is the lower-level form the agent itself uses.
```


### `cowboy ranch accept`

Sign off on a workstream waiting at its acceptance gate (unblocks deps)

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `<WORKSTREAM>` |  |


### `cowboy ranch add`

Add a workstream to a ranch (no hand-editing ranch.yaml). Rejects a dependency cycle or an unknown `--depends-on` id

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `<WORKSTREAM_ID>` | Workstream id (short, unique within the ranch) |
| `--goal` | What this workstream should accomplish |
| `--title` | Display title (defaults to the workstream id) |
| `--depends-on` | Workstream ids this one depends on (comma-separated) |
| `--acceptance` | Acceptance criteria, human-readable (comma-separated) |
| `--expects` | Expected artifact name(s) this workstream should publish (repeatable) |


### `cowboy ranch approve`

Approve a pending proposal: apply its change to the plan

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `<PROPOSAL>` |  |


### `cowboy ranch attach`

Attach the TUI to a workstream's running session

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `<WORKSTREAM>` |  |


### `cowboy ranch complete`

Mark a workstream complete (promotes its artifacts + unblocks dependents)

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `<WORKSTREAM>` |  |


### `cowboy ranch create`

Create a new ranch plan (writes a skeleton ranch.yaml to fill in)

| Arg | Description |
|-----|-------------|
| `<TITLE>` | The ranch's title (also seeds its id) |
| `--goal` | The overall goal |


### `cowboy ranch draft`

Draft a ranch from a decomposition spec file (YAML/JSON with `title`, `goal`, and a `workstreams` list). Validates the dependency DAG and writes the draft ranch.yaml. Used by `cowboy ranch plan`: the agent authors the spec with the `write` tool, then runs this to validate and draft it

| Arg | Description |
|-----|-------------|
| `<SPEC>` | Path to the decomposition spec (YAML or JSON) |


### `cowboy ranch plan`

Decompose a goal into a ranch plan with the agent: it researches the codebase read-only and proposes workstreams + dependencies for you to review (writes a draft ranch.yaml; starts nothing)

| Arg | Description |
|-----|-------------|
| `<GOAL>` | The overall goal to decompose into workstreams |


### `cowboy ranch proposals`

List a ranch's scope-change proposals

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `--all` | Include already-decided proposals (default: pending only) |


### `cowboy ranch propose`

Propose a scope change to the plan (recorded as pending; needs approval)

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `--summary` | One-line summary of the proposal |
| `--rationale` | Why this change is needed |
| `--add-workstream` | Propose adding a workstream with this id |
| `--remove-workstream` | Propose removing this (not-yet-started) workstream |
| `--note` | File a free-form note/concern (no automatic edit) |
| `--title` | Title for an added workstream |
| `--goal` | Goal for an added workstream |
| `--depends-on` | Dependencies for an added workstream (comma-separated ids) |


### `cowboy ranch reject`

Reject a pending proposal (records the decision; plan unchanged)

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `<PROPOSAL>` |  |
| `--reason` | Why it was rejected. Recorded with the decision and shown to the workstream that proposed it, so it can try something else |


### `cowboy ranch retry`

Reset a failed or interrupted workstream so it re-runs on the next start

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `<WORKSTREAM>` |  |


### `cowboy ranch start`

Launch ready workstreams (deps complete), each in its own worktree/branch. Re-run as workstreams finish to advance the plan

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |


### `cowboy ranch status`

Show ranch status: all ranches, or one with its workstreams

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |


### `cowboy ranch watch`

Live TUI dashboard: watch workstreams advance, start/refresh from keys

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |


## `cowboy replay`

Replay or inspect a previous session

| Arg | Description |
|-----|-------------|
| `<SESSION_ID>` |  |


## `cowboy review`

Read-only review of a session's output (or a branch): prints a bundle and records a Review artifact. Never edits anything

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |
| `--branch` | Review a branch's changes instead of a session |


## `cowboy run`

Run a command inside the agent sandbox

| Arg | Description |
|-----|-------------|
| `<COMMAND>` | The command and its arguments |

```text
Examples:
  cowboy run cargo test  # run it under the same confinement the agent gets
  cowboy run -- ls -la   # use -- when the command has its own flags

There is no network unless the project's security.yaml allows the destination.
```


## `cowboy sandbox`

Inspect the sandbox boundary for this project

```text
Examples:
  cowboy sandbox plan          # what the agent can read, write and reach
  cowboy sandbox exec cargo test

`plan` is the honest answer to "what is the agent allowed to do here?" — it is rendered
from the same pure logic the session builds the boundary from, not a separate summary.
```


### `cowboy sandbox exec`

Run one command inside the sandbox, with no network access

| Arg | Description |
|-----|-------------|
| `<COMMAND>` | The command and its arguments |


### `cowboy sandbox plan`

Print the confinement plan for this project: what the agent can read, write, and reach, and which paths can never be granted at runtime


## `cowboy secrets`

Grant host credentials (gh, gcloud, kubectl, …) into the sandbox


### `cowboy secrets add`

Add a credential grant (a known preset and/or explicit env/file grants) to your personal host-side overlay; --repo prints a snippet to paste instead

| Arg | Description |
|-----|-------------|
| `<PRESET>` | A known tool preset: gh, gcloud, kubectl, aws, git, ssh |
| `--env` | Grant an env var into the sandbox: `NAME` or `NAME=HOST_ENV` |
| `--file` | Grant a host file/dir read-only: `SRC` or `SRC:CONTAINER_TARGET` |
| `--global` | Write to the cross-project user overlay instead of this worktree's |
| `--repo` | Print a snippet to paste into the repo's .cowboy/security.yaml instead of writing your personal (home-dir) overlay |

```text
Examples:
  cowboy secrets add gh                       # a known preset (gh, gcloud, kubectl, aws, git, ssh)
  cowboy secrets add --env GITHUB_TOKEN       # pass a host env var through by name
  cowboy secrets add --env TOKEN=MY_HOST_VAR  # ...under a different name inside
  cowboy secrets add --file ~/.netrc          # bind a host file read-only
  cowboy secrets add gh --global              # every project, not just this one
  cowboy secrets add gh --repo                # print a security.yaml snippet instead of writing

Values are resolved host-side. The overlay lives in ~/.config/cowboy/secrets/, which the
agent cannot write.
```


### `cowboy secrets list`

Show configured credential grants and whether each host source exists


## `cowboy session`

Inspect and maintain sessions (list, reap stale records and their leases)


### `cowboy session cleanup`

Reap stale (crashed/abandoned) session records and release their leases. Worktrees and branches are never touched

| Arg | Description |
|-----|-------------|
| `--dry-run` | Show what would be reaped without changing anything |


### `cowboy session list`

List sessions tracked by the daemon (same as `cowboy sessions`)


## `cowboy sessions`

List sessions tracked by the daemon


## `cowboy shell`

Open an interactive shell inside the agent sandbox


## `cowboy skill`

List or show agent skills (reusable instructions under .cowboy/skills/)


### `cowboy skill list`

List available skills (name + description)


### `cowboy skill show`

Print a skill's instructions (to follow / pull into context)

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


## `cowboy web`

Serve a web UI to attach to running sessions from a browser (e.g. a phone over Tailscale). Binds loopback by default; token-authenticated

```text
Examples:
  cowboy web on                          # loopback only
  cowboy web on --bind 100.x.y.z:7777    # a Tailscale address
  cowboy web status                      # the URL, plus a QR code for a remote bind
  cowboy web off
```


### `cowboy web off`

Disable the web UI and stop the daemon serving it


### `cowboy web on`

Enable the web UI and have the daemon start serving it

| Arg | Description |
|-----|-------------|
| `--bind` | Address to bind, e.g. `127.0.0.1:8787` or your Tailscale IP `100.x.y.z:8787`. Persisted; defaults to `127.0.0.1:8787`. Non-loopback/non-Tailscale binds are refused unless `--lan` is set |
| `--lan` | Permit a non-loopback, non-Tailscale bind (LAN / `0.0.0.0`). The token then travels in cleartext — only use on a trusted network |


### `cowboy web status`

Show whether the web UI is enabled + serving, with its URL (and a QR for a remote bind)


## `cowboy worktree`

List or create git worktrees for parallel sessions

```text
Examples:
  cowboy worktree create "fix login"      # make cowboy/fix-login and a branch for it
  cowboy worktree list                    # which worktree each session is holding
  cowboy worktree status cowboy/fix-login # is it mergeable into HEAD?
  cowboy worktree diff --session 1788401869978-1

Running `cowboy` inside a worktree confines the agent to that worktree, so two sessions
can work the same repo without stepping on each other.
```


### `cowboy worktree create`

Create a `cowboy/<slug>` worktree off the current repo

| Arg | Description |
|-----|-------------|
| `<NAME>` | Task/branch hint used for the slug (e.g. "fix login") |


### `cowboy worktree diff`

Show a branch's diff stat vs its fork point (read-only)

| Arg | Description |
|-----|-------------|
| `<BRANCH>` | Branch to inspect (or use --session) |
| `--session` | Resolve the branch from a session id instead |


### `cowboy worktree list`

List git worktrees and any session occupying each


### `cowboy worktree status`

Summarize a branch's changes + mergeability vs HEAD (read-only)

| Arg | Description |
|-----|-------------|
| `<BRANCH>` | Branch to inspect (or use --session) |
| `--session` | Resolve the branch from a session id instead |


## `cowboy x-fileop`

Internal: in-sandbox worker for the structured file tools (reads a JSON request on stdin). Not for direct use


## `cowboy x-sandbox-holder`

Internal: holds a session's namespaces open. Runs inside them, brings loopback up, and exits when its stdin closes


## `cowboy x-sandbox-shim`

Internal: the in-sandbox shim that applies Landlock + seccomp then execs the agent's command. Reads its request from stdin as JSON


## `cowboy x-session-worker`

Internal: headless session worker spawned by the daemon. Not for direct use

| Arg | Description |
|-----|-------------|
| `--root` | Worktree root the session runs in |
| `--task` | Optional initial task |
| `--sock` | Override the per-session socket path |
| `--id` | Daemon-assigned session id (used for the session dir + registry) |
| `--register` | Register with (and heartbeat to) the daemon |
| `--resume` | Continue a prior session: load its transcript as the starting history |
| `--ranch-id` | Tag this session as a Ranch workstream |
| `--workstream-id` | Which workstream of `--ranch-id` this session is running |

