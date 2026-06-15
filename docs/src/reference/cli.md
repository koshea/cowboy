# CLI reference

<!-- GENERATED from the clap command tree by `cargo test -p cowboy-cli --test cli_docs`.
     Do not edit by hand. Regenerate with:
     COWBOY_REGEN_DOCS=1 cargo test -p cowboy-cli --test cli_docs -->

An opinionated local coding agent that runs wild inside a Docker corral.

## `cowboy` (global options)

| Arg | Description |
|-----|-------------|
| `<TASK>` | Optional one-shot task. With no subcommand, `cowboy 'fix the tests'` starts a session with the task prefilled |
| `-v, --verbose` | Enable debug logging (or set COWBOY_LOG=...) |
| `--attach-if-active` | On a same-worktree collision, attach to the active session instead of prompting |
| `--read-only` | On a same-worktree collision, attach read-only (watch without driving) |
| `--new-worktree` | On a same-worktree collision, create a new git worktree and run there |
| `--force-same-worktree` | Take over a *stale* lease on this worktree (never a live one) |
| `--continue` | Continue the most recent session in this worktree, keeping its history |
| `--resume` | Resume a specific session by id, keeping its conversation history |


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
| `--kind` | Kind: contract\|summary\|patch\|diff\|test_result\|notes\|review\|other |
| `--title` | Friendly title (defaults to the file name) |
| `--summary` | One-line summary |
| `--session` |  |


### `cowboy artifact list`

List artifacts for a session (defaults to the most recent)

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |


### `cowboy artifact show`

Print an artifact's body by id

| Arg | Description |
|-----|-------------|
| `<ID>` |  |
| `--session` |  |


## `cowboy attach`

Attach the TUI to a running session (by id, or a worker socket path)

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |


## `cowboy crew`

Manage the Crew Roster (route delegated work to models by category/effort)


### `cowboy crew init`

Write a default crew roster (tiers derived from your models' prices)

| Arg | Description |
|-----|-------------|
| `--force` | Overwrite an existing crew.yaml |


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
| `<ID>` |  |
| `--session` |  |


## `cowboy doctor`

Check Docker, Linux support, model config, network gateway, and Compose


## `cowboy down`

Stop and remove this project's agent + gateway containers and networks

| Arg | Description |
|-----|-------------|
| `--all` | Remove ALL cowboy-managed containers and networks (every project) |


## `cowboy handoff`

Print a session's handoff summary (defaults to the most recent)

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |


## `cowboy inbox`

Read (and drain) a session's message inbox (defaults to the most recent)

| Arg | Description |
|-----|-------------|
| `<SESSION>` |  |


## `cowboy init`

Create initial project config files under `.cowboy/`

| Arg | Description |
|-----|-------------|
| `--force` | Overwrite existing config files if present |
| `--git` | Also run `git init` if the project is not already a git repository |


## `cowboy logs`

List session logs


## `cowboy memory`

Inspect the agent's saved memory (project + global)


### `cowboy memory delete`

Delete a memory by name

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


## `cowboy models`

Configure model providers (home-owned) and models


### `cowboy models add`

Register a model by its provider id, prefilled from shipped defaults

| Arg | Description |
|-----|-------------|
| `<ID>` | The provider-side model id, e.g. `cerebras/zai-glm-4.7` |
| `--name` | Friendly name (config key). Defaults to the recommended name |
| `--provider` | Provider to use (defaults to the only configured one) |
| `--temp` |  |
| `--context` |  |
| `--max-output` |  |
| `--reasoning` | Reasoning effort: none\|minimal\|low\|medium\|high |
| `--default` | Make this the default model |


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

Patch helper (wraps git inside the container)


### `cowboy patch apply`

Apply a patch read from stdin


### `cowboy patch check`

Validate that a patch from stdin applies cleanly


### `cowboy patch revert`

Revert uncommitted changes (asks for confirmation)


### `cowboy patch save`

Save the current git diff to the session `diff.patch`


### `cowboy patch show`

Display the current git diff


## `cowboy proc`

Managed long-running process commands


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

Start a process by name

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


### `cowboy proc stop`

Stop a process by name

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


## `cowboy ranch`

Create or inspect Ranch Plans (multi-workstream tasks)


### `cowboy ranch accept`

Sign off on a workstream waiting at its acceptance gate (unblocks deps)

| Arg | Description |
|-----|-------------|
| `<RANCH>` |  |
| `<WORKSTREAM>` |  |


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
| `--reason` |  |


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

Run a command inside the agent container

| Arg | Description |
|-----|-------------|
| `<COMMAND>` | The command and its arguments |


## `cowboy secrets`

Grant host credentials (gh, gcloud, kubectl, …) into the container


### `cowboy secrets add`

Print a paste-ready grant (a known preset and/or explicit env/file grants) to add to .cowboy/security.yaml. Non-destructive

| Arg | Description |
|-----|-------------|
| `<PRESET>` | A known tool preset: gh, gcloud, kubectl, aws, git, ssh |
| `--env` | Grant an env var into the container: `NAME` or `NAME=HOST_ENV` |
| `--file` | Grant a host file/dir read-only: `SRC` or `SRC:CONTAINER_TARGET` |
| `--global` | Write to the cross-project user overlay instead of this worktree's |
| `--repo` | Print a snippet to paste into the repo's .cowboy/security.yaml instead of writing your personal (home-dir) overlay |


### `cowboy secrets list`

Show configured credential grants and whether each host source exists


## `cowboy session`

Session maintenance (reap stale records and their leases)


### `cowboy session cleanup`

Reap stale (crashed/abandoned) session records and release their leases. Worktrees and branches are never touched

| Arg | Description |
|-----|-------------|
| `--dry-run` | Show what would be reaped without changing anything |


## `cowboy sessions`

List sessions tracked by the daemon


## `cowboy shell`

Open an interactive shell inside the agent container


## `cowboy skill`

List or show agent skills (reusable instructions under .cowboy/skills/)


### `cowboy skill list`

List available skills (name + description)


### `cowboy skill show`

Print a skill's instructions (to follow / pull into context)

| Arg | Description |
|-----|-------------|
| `<NAME>` |  |


## `cowboy worktree`

List or create git worktrees for parallel sessions


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
| `<BRANCH>` |  |
| `--session` |  |


## `cowboy x-fileop`

Internal: in-container worker for the structured file tools (reads a JSON request on stdin). Not for direct use


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
| `--workstream-id` |  |

