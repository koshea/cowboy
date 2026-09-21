# The agent & its tools

The agent loop calls an OpenAI-compatible model with a fixed tool surface. Some
tools run **inside the sandbox**; others are **host-handled** by the loop (the
agent can't reach the host directly, so the loop does it).

## Tool surface

| Tool | Where | What it does |
|------|-------|--------------|
| `shell` | sandbox | Run a command in the sandbox; output is streamed back, with its exit code and how long it took. Pass `timeout_seconds` to raise or lower the per-call timeout (a slow test suite vs. a command that should be quick). Each call is a fresh shell — see [the shell contract](#the-shell-contract). Other Cowboy CLIs (`patch`, `skill`) are invoked *through* `shell`. |
| `read` / `edit` / `write` | sandbox | File operations in the workspace. `edit` takes a batch of `edits` applied all-or-nothing, repairs a mechanically-wrong match and diagnoses the rest (see [below](#file-edits)), and reports the applied diff back. `write` refuses a blind or stale overwrite. `read` refuses a binary file with a clear message instead of a UTF-8 error. |
| `grep` | sandbox | Search the workspace for a regex, reporting `path:line:text`. Skips build output, dependency trees, `.gitignore`d paths and binaries; reports true match totals. Works in plan mode. |
| `ls` | sandbox | List a directory's entries (directories suffixed `/`). Skips build/dependency trees and `.gitignore`d paths, caps the listing, reports the true total. `recursive` walks the tree; `glob` filters files. Works in plan mode. |
| `proc` | host | Start/stop/inspect a long-running background process — a dev server you then send requests to. See [background processes](#background-processes). |
| `memory` | host | Save/recall durable project memory. |
| `plan` | host | Maintain a working to-do plan, surfaced in the UI; drives lifecycle events. |
| `artifact` | host | Publish a named output (contract, summary, review, …) into the session's artifact store. |
| `handoff` | host | Write a structured handoff summary (`handoff.md`) at the end of a session. |
| `decision` | host | Record a decision (question, options, choice, rationale) durably. |
| `blocked` / `unblock` | host | Declare/clear a "cannot proceed" state, surfaced to the user and the Ranch coordinator. |
| `propose_scope_change` | host | (Ranch only) File a pending change to the ranch plan for the user to approve — the agent never edits the plan directly. |
| `request_path` | host | Ask the user for access to a host path outside the workspace. Approved paths apply to the *next* command. Credential stores are always refused. |
| `final` | — | Finish the current *turn* with a summary. |
| `ask_user` | host | Ask the user a question, optionally with selectable options. |
| `subagent` | host | Delegate a focused sub-task to a fresh subagent in the same sandbox. **Asynchronous**: returns a job id, and the result is delivered as a message when the job finishes. |
| `jobs` | host | List the background subagent jobs, with each worker's turn usage. |
| `wait` | host | Park until a background job reports. Bounded, and broken by an interrupt or by anything you type. |
| `job_reply` | host | Answer a worker that asked for more turns: `grant`, `redirect`, `wrap_up`, or `stop`. |
| `request_turns` | host | (Workers only) Report progress and ask the foreman for more turns. |

The exact, current list is asserted by a test and rendered in the
[CLI reference](../reference/cli.md) companion; adding a tool follows the pattern
documented in `AGENTS.md`.

## Finding code

`grep` is a real tool rather than a `shell` incantation, for three reasons that
each cost a wasted turn otherwise. The sandbox re-binds *your* `/usr`, so `rg`
exists only if you happen to have installed it — a search tool that silently
degrades to something worse is not a search tool. A plain `grep -r` descends into
`target/` and `node_modules/` and buries the answer. And when output overruns the
cap, the agent cannot tell whether it saw 10 matches or 10 000.

So `grep` skips build output, dependency trees, VCS metadata, anything
`.gitignore` excludes and binary files, clips absurdly long lines, prints a
bounded number of matches — and always reports the **true total**, so an
over-broad pattern produces a count and a hint to narrow it instead of a wall of
text. `pattern` is a regex; pass `literal: true` for plain text, `glob` to filter
by filename or path (`*.rs`, `src/**/mod.rs`), and `path`
to scope it to a subtree. `context: N` prints `N` lines on each side of a match
(match lines use `path:line:`, context lines `path:line-`, like `grep -C`), and
`files_only` reports just the matching file paths when you only need to know
*where* something is. An explicitly requested skipped directory (`path:
"target"`) *is* searched — asking for it is the point.

`ls` is the same idea for directory listings: `ls`/`find` via `shell` either
descend into `target/` and `node_modules/` and flood the context, or are missing
entirely, and neither reports a bounded total. The `ls` tool skips the same
build/dependency trees, caps its output and reports the true count. It lists one
level by default; `recursive: true` walks the whole tree, `glob` filters files,
and `path` scopes it to a subdirectory. Directories are suffixed with `/`.

Being read-only, `grep` and `ls` are available in **plan mode**, where `shell` is
blocked. Before they existed, a planning agent was told to investigate and had no
way to search or browse.

### `.gitignore` is the skip list

The built-in skip list (`target`, `node_modules`, `.venv`, `dist`, …) is a fixed
list and therefore wrong for some repo: build output in `out/`, `_build/` or
`coverage/`, a vendored dependency tree, generated clients. Your repo already
declares that set, so both tools honour **`.gitignore`** as well — the root one and
any nested ones, with `!` negation and anchoring read the way git reads them.

When something was hidden, the result says so, because otherwise a search that
legitimately targets generated code looks like a search that found nothing. Pass
`include_ignored: true` to search it anyway.

## File edits

`edit` replaces an exact, unique span. Three things make the failures cheap:

**Batches are transactional.** Pass `edits: [{old, new}, …]` to change a file in
several places. They apply in order — each sees what the previous one produced —
and the file is written **once**, at the end, only if all of them succeed. A batch
that fails on its third edit leaves the file untouched, rather than half-applied
with the agent left to work out which half landed.

**A mechanical mismatch is repaired, not reported.** Three ways an `old` misses have
exactly one correct reading, and in those cases the edit simply lands:

- the **line-number gutter** from `read` output was copied along with the text (by
  far the most common miss) — it is stripped, from `new` as well when it is there
  too, since writing that back would put line numbers *into* the file;
- the **line endings** differ from the file's — `old` and `new` are both converted
  to the file's convention;
- the block is **uniformly indented** differently — it matches, and the replacement
  is re-indented to the file's depth rather than the model's.

Each is reported in the result (`note: …`), so the agent knows what actually reached
the file. Nothing fuzzy is repaired: a non-uniform whitespace difference, or two
candidate windows instead of one, stays an error, because applying a *guess* to the
wrong span is far worse than costing a turn.

**A failed match explains itself.** "Not found, copy the exact text" is true and
useless; the retry fails the same invisible way. The error names the specific cause
where there is one — whitespace that differs non-uniformly (with the real text, at
its line number), surrounding blank lines — and names what it already ruled out
(the gutter, the line endings) so a compounded failure does not look like a simple
one. Failing that, it reports the closest block in the file as a diff against what
was asked for. An ambiguous `old` reports the **line numbers** of every match, not
just how many there were.

**The result carries the diff.** A successful `edit` or `write` reports a few lines
of unified diff of what changed, with context. "edited x.rs: 1 replacement" is true
and says nothing about *where* the text went, so a careful agent re-read the file
(a whole round trip for a change it had just made) and a careless one carried on
against an assumed result.

Writes are atomic (temp file + rename) and preserve the file's permissions, so a
watcher or a build running concurrently never sees a half-written file, and
overwriting a script does not quietly drop its executable bit.

### Overwrites must not be blind

`write` replaces a whole file and has no `old` to guard it, so unlike `edit` it
cannot fail on a stale assumption — it silently wins. Two ways that loses work
here: subagents are dispatched **in parallel into the same workspace**, so two
workers touching one file is ordinary; and a build, codegen step or formatter the
agent itself started rewrites files under it.

So a `write` over an existing file is refused unless the bytes on disk are the bytes
this session last saw — either because it `read` the file, or because it wrote the
file. The refusal names the reason and the fix (`read` it, or use `edit` to change
only your part), and one read clears it. Creating a new file is never affected.

## The shell contract

Every `shell` call is a **fresh process in a fresh sandbox**. A `cd` or an `export`
does not carry to the next call — pass `cwd`, or chain with `&&`. The filesystem
and anything left listening on localhost *do* persist. The command runs under
`/bin/sh -c`, which is dash on Debian/Ubuntu, so POSIX only.

That was always true, and used to be something an agent discovered by getting a
wrong answer from a command that ran in the wrong directory. It is now stated in
the tool's own schema, where it is read before the first mistake rather than after.

Each result reports the exit code **and the duration** — the model has no clock, and
the difference between a 0.2s test and a 9-minute one decides whether to re-run the
whole suite or pick a bigger `timeout_seconds`.

A command stopped by its timeout is not a real exit status, so it says so rather
than handing over a bare `124`: it names the timeout that fired and both ways out —
raise `timeout_seconds` if the work genuinely takes longer, or stop running a server
in the foreground and use `proc`.

## Background processes

Testing a service means starting it and then talking to it, and a `shell` call
cannot hold a server: each one gets its own sandbox whose entire process tree is
reaped when the command returns. `&`, `nohup` and `setsid` do not escape that —
bwrap is PID 1 of the command's own PID namespace, so the kernel kills everything
in it. (`cowboy proc start` did exactly this, and reported success for a process
that was already dead; it now says why it cannot, and starting one lives here.)

The `proc` tool starts a process owned by the **session**:

- it is confined exactly like any other command, by the same plan;
- it shares the session's network namespace, so later `shell` commands reach it on
  localhost;
- it gets its own PID namespace, so stopping it reaps precisely its own tree;
- its combined output goes to `.cowboy/proc/<name>.log` — a file the agent can
  `read` and `grep`, rather than a dev server's chatter in the transcript;
- it **dies with the session**, which is the right lifetime for a dev server and the
  reason a short-lived CLI cannot own one.

`proc start <name>` needs a `command` unless the name is declared in
`.cowboy/agent.yaml` under `processes:`, in which case the command and `cwd` come
from there. Declared processes are listed to the agent, and one marked
`auto_start: true` is running before the first turn.

## Verification

By default, whether to run the tests is the model's judgement. A project can make
it a requirement instead, in `.cowboy/agent.yaml`:

```yaml
agent:
  verify:
    - test          # a `commands:` key…
    - cargo clippy --workspace   # …or a literal command
commands:
  test: cargo nextest run
  lint: cargo clippy
```

`commands` is shown to the agent so it runs *your* checks rather than guessing an
invocation from the language — which is how a repo whose tests need a codegen step
gets `cargo test` run directly and the step skipped. The list rides in the pinned
system message, so it is still there on turn 50.

`verify` additionally gates completion: if the session changed files and a required
check has not passed **against the current tree**, `final` is refused and the agent
is told exactly what to run. The evidence is recorded host-side from actual exit
codes, so it cannot be satisfied by asserting it in the summary; any edit
invalidates an earlier pass, and a failing run does not count.

The gate is deliberately soft, and is **not** part of the security boundary. It
refuses a couple of times and then yields with a notice, exactly like the
outstanding-subagent gate — a check that genuinely cannot run here (no network, a
missing toolchain) must not be able to trap a session. Sessions that changed
nothing are never gated, so questions and reviews are unaffected. With no `verify`
configured there is no gate at all.

## What the agent starts a session knowing

Three things are **pinned** into the system message, so they cost no turn to obtain
and survive context compaction:

- **`AGENTS.md` (or `CLAUDE.md`)** from the repo root. The agent is told this file is
  authoritative, and the alternative was spending a turn reading it at the start of
  every session — then losing it to compaction halfway through a long one, exactly
  when there is the most accumulated code to stay consistent with. Bounded by
  `agent.project_instruction_bytes` (12 000 by default, `0` to turn it off) because it
  is paid for on every request of every session, including each parallel subagent's; a
  longer file is clipped with a note saying to read the rest. Nested `AGENTS.md` files
  are still the agent's to find when it works in a subtree.
- **The skill index** — one line per skill, name and description. Discovery via
  `cowboy skill list` alone meant a turn spent finding out whether skills existed at
  all, so a prompt that said they "may be available" got that turn spent on most
  sessions and skipped on the rest. The instructions — the expensive half — are still
  fetched with `cowboy skill show <name>` on demand.
- **`commands:`, `verify:` and `processes:`** from `agent.yaml`, as above.

Plus the **memory index**, which already worked this way.

Repo files are of course repo content, and therefore untrusted. That is not a new
exposure: it is the same bytes the agent would `read` a turn later, and nothing in
the boundary depends on what the model is told — mounts, network and credentials are
enforced host-side against config the agent cannot reach. See
[the security model](../security/model.md).

## The sandbox environment

The agent gets **your** toolchain. `/usr` and `/opt` are exposed read-only, so
whatever compilers, runtimes and CLIs you have installed are what the agent runs,
at your versions — there is no image to build, pull, or keep in step. Your project
is writable at `/workspace`; the rest of the machine is absent.

Run `cowboy sandbox plan` to see the exact list for your machine.

> Working in a **git worktree**? Cowboy detects it and also exposes the main
> repository's git directory, so `git` (status/diff/log/commit) works even though
> the worktree's `.git` points outside `/workspace`.

If a command fails because something outside the project is missing, the agent can
call `request_path` to ask for it, or you can run `cowboy grant <path>`. Either way
the next command sees it, with no restart. Credential stores are refused whatever
the reason given — use `cowboy secrets add` for those.

### Managing dependencies with mise (recommended)

[mise](https://mise.jdx.dev/) is the **preferred way to manage per-project dev
dependencies** (language runtimes, CLIs, env vars) that the host does not already
provide. Install it on the host and:

- When the workspace has a mise config (`mise.toml`, `.mise.toml`,
  `.config/mise/config.toml`, `.tool-versions`, …), Cowboy runs **`mise install`
  automatically at launch** — so a freshly-created worktree comes up with its
  declared toolchain ready, no manual step.
- The workspace is trusted automatically
  (`MISE_TRUSTED_CONFIG_PATHS=/workspace`), and mise's shims are on `PATH` for
  both the agent's commands and an interactive `cowboy shell`.

- Your own mise store (`~/.local/share/mise`) is shared **copy-on-write**, so a
  toolchain you already installed is reused instead of downloaded again. This only
  applies to projects that use mise (one of the config files above); a project
  without a mise config gets no store and no `.cowboy/mise/` directory. The
  sandbox reads your store and writes to `.cowboy/mise/` in the project (git-ignored),
  so it can install a version you do not have — and your store is never written to.
  Turn it off with `share_mise_store: false`; it also needs `host_tools`.

Commit a mise config to your repo and the agent gets a consistent, reproducible
toolchain every session.

## Context management

Everything sent to the model competes for one window, and the loop's job is to keep the
request inside it while losing as little as possible.

**The budget.** The window is not all yours: room is reserved for the model's reply, for
the tool schemas (~3.6k tokens, sent on *every* request), and for a small headroom
floor. What is left is the conversation's budget. If the window cannot even hold the
reserve, Cowboy says so — naming the window, the model's `max_tokens` and the schema
cost — rather than letting the request fail at the provider with an error that explains
none of that.

**`/context`** shows where you stand at any time, including mid-turn:

```
context  84,500/160,000 tokens of the conversation budget (52%)
         window 200,000 · reserved 40,000 for the reply, tool schemas and headroom
         largest first:
           tool results             41,000  █████████
           model reasoning          22,000  █████
           assistant messages       12,500  ██
           tool schemas              3,650
```

Grouped by what produced it, because the useful question is which *kind* of thing is
filling the window.

**What is shed, in order of cheapness.** Reasoning models return their thinking, and it
is sent back on every subsequent request to keep them on plan across tool calls — but
only the last couple of turns need it, so older reasoning is dropped before each call.
This is free and happens first, which often means there is nothing left to compact. Then
every tool result is capped (`agent.max_command_output_bytes`, 60 KB) — all of them,
including subagent answers and MCP responses. Only if the conversation still overflows
does Cowboy compact: the oldest whole turns are folded into a model-written summary
(itself capped, so a fold always shrinks). Dropping history without summarizing is the
last resort, and it says so each time it happens, with a count.

**What never goes.** The system prompt and your current task statement. The task is
tracked by content, so it survives every fold and prune wherever it sits — including
after `--resume`, where the previous session's transcript sits in front of it. A resume
loads at most half the budget, newest first, so continuing an old session cannot blow
the window on the first request.

**Tool calls always keep their results.** Every trim, fold and prune above can in
principle cut across a turn boundary, and the shape that results — an assistant turn
whose tool call has no result, or a result whose call is gone — is one providers reject
outright. Because the whole conversation is replayed on every request, that would not
fail one turn but every turn after it. So the pairing is repaired in the one place that
matters, immediately before each model call, rather than each trimming path being
separately trusted not to break it. A repair still means information was lost, so it
logs; it just cannot brick the session.

If a reasoning model burns its whole output budget thinking and returns no answer
or tool call, Cowboy warns that its `max_tokens` may be too low and then recovers
rather than ending the turn. It retries with a directive to answer now, and asks the
provider for **minimal reasoning effort** on that retry — telling a reasoning model
not to think is advice it can ignore, so the knob the provider honours is turned as
well. When the provider returned the cut-off reasoning, it is first distilled into
conclusions-so-far so the retry builds on them instead of re-deriving them; when it
did not — many providers bill reasoning tokens without ever sending the text — the
retry goes ahead anyway, because the model still has the whole transcript.

Recovery is bounded (`2` attempts). Only after those are spent does the turn end,
with an `[incomplete]` result naming the two levers you have: raise `max_tokens` or
lower `reasoning_effort`. The low-effort request lasts for the retry only, so a model
that recovers keeps its normal reasoning for the rest of the session. Both the
compaction and recovery summaries use the optional
[`summarizer`](../getting-started/configuration.md) model when configured,
falling back to the main model otherwise, and always request minimal reasoning —
summarizing is mechanical, and a model that just truncated while thinking would
otherwise do the same on the summary and come back empty.

## What a session records

Under `.cowboy/sessions/<id>/`:

- **transcript / command logs / diff** — the raw run.
- **`artifacts/` + `artifacts.jsonl`** — published outputs (the `artifact` tool).
- **`handoff.md`** — the session's headline summary (auto-generated if the agent
  didn't publish one).
- **`lifecycle.jsonl`** — semantic events (plan steps, artifacts, blocked/
  unblocked, decisions, completion) consumed by the Ranch coordinator.
- **`decisions.jsonl`** — recorded decisions.

These outputs are what makes [Ranch Plans](../ranch/overview.md) coordinate
through artifacts rather than chat.
