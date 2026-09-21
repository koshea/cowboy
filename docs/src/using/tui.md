# The TUI

On a terminal, `cowboy` runs a ratatui-based terminal UI that streams the agent's
work turn by turn.

## Layout & interaction

- A scrollable transcript pane shows the conversation, tool calls, and streamed
  command output; the input box at the bottom grows up to 5 lines as you type.
- **Enter** sends; multi-line input is supported (paste arrives as one chunk).
- **Typing while the agent works steers the running turn** — the message reaches
  the agent at its next step instead of waiting for the turn to end. `/after <msg>`
  queues one to run afterwards instead; `/queue` shows what is waiting.
- **Ctrl-C** cancels the current turn and puts the cursor in the input so you can
  redirect it; background subagents keep running. At an empty idle prompt, two
  presses end the session.
- **Direct keys** replace what used to be a Ctrl-C menu: **Alt-j** lists the
  subagents, **Alt-w** watches one, **Alt-s** stops them, **Alt-f** folds the turns
  you have already read, **Alt-d** detaches.
- **F1** opens the keys-and-commands reference, scrollable and grouped; `/help
  <topic>` filters it (`/help subagent`).
- The status bar shows background work and deferred input (`3 jobs (1 asking) ·
  1 queued`), so neither is invisible while the agent is busy. A subagent that has
  spent its turn grant and is waiting for a decision shows as `⏸ … wants +N turns`
  in the background pane.
- **Scrolling** follows the tail by exact wrapped-line counts, so the latest
  output is never cut off under the input box.
- **`/help`** lists the slash commands; **`/context`** shows how much of the model's
  window the conversation is using and what is filling it (see
  [Context management](agent-and-tools.md#context-management)), and completion
  suggestions appear as you type `/`.

## The session banner

A new interactive session opens with a short branded intro animated at the top of
the transcript, where the welcome text used to start. It is part of the transcript
rather than a fixed header, so it scrolls away with the rest of the welcome block
once the work starts. Its accent colour is derived from the project path, which
means every repo (and every worktree) has a stable colour: a glance at a window
tells you which one you are in.

Any key or click settles it immediately, and `COWBOY_BANNER` controls it:

| Value | Effect |
|---|---|
| unset | play the animation, then settle |
| `static`, `plain`, `1` | show the settled frame only, no motion |
| `off`, `0`, `false`, empty | show nothing |

It is also skipped automatically where it would be wrong or pointless: under
`NO_COLOR`, in a transcript pane too narrow for the art, and in one too short to
show it beneath the welcome lines — on a small pane the wordmark would be pushed
straight off the top, so nothing is drawn instead.

While it animates the event loop polls at the animation's frame interval and then
drops back to its normal idle cadence, so the smoothness costs nothing for the rest
of the session. The animation is the one thing that redraws the transcript every
frame, and it is only affordable because at session start the transcript *is* the
welcome block — which is why nothing else animates through it.

Beneath the project and model lines, a fresh session offers a few **openers**
derived from what the repo already declares (its `commands`/`verify`, a root
`AGENTS.md`, whether the worktree is dirty). **Alt-1**…**Alt-9** sends one. They
are offered only when you started without a task — if you already said what you
wanted, suggestions are noise.

## Reading a long session

The transcript is a conversation with the mechanics in between. `/fold` (or
**Alt-f**) collapses every finished turn to a single line:

```text
▸ folded — 6 commands, 4 file actions · 128 lines hidden · /unfold or Alt-f
```

Prompts, answers and **errors always stay visible** — folding hides the work, not
the conversation, and a collapsed turn must never be able to swallow the reason it
failed. The turn in progress is never folded: it is the one you are reading.
`/unfold` (or Alt-f again) expands everything.

## What the status bar tells you

Beyond the mode and the running command's tail:

- `🔒 egress ask` — the boundary indicator: the project's default verdict for
  external destinations. `/boundary` prints the whole thing (mounts, Landlock,
  seccomp, never-grantable paths, and the egress policy in force), from the same
  code as `cowboy sandbox plan`.
- `ctx ▰▰▰▰▱▱▱▱ 52%` — how much of the conversation budget the next request will
  occupy. It turns amber at 70% and red at 90%, and from amber on it names the
  largest consumer, because compaction starts folding away older turns *before*
  the budget is literally exhausted. `/context` has the full breakdown.
- `⏳ your turn — answer above · waiting 45s` — the session is parked on you, and
  for how long. A background job asking for turns does **not** show here (the turn
  is still running); it shows in the jobs segment.

## When the session needs you

A question, a choice, a network approval, or a job asking for more turns are all
pauses that fail closed — with no answer, nothing proceeds. Since the status bar
is no help when the window is not focused, the TUI also rings the terminal bell,
sends a desktop notification (OSC 9, where the terminal implements it), and marks
the window title `⏳ cowboy needs you — …` until the wait ends.

The title is restored on exit via the terminal's own title stack, so quitting
never leaves your terminal renamed. Set `COWBOY_NOTIFY=0` (or `off`/`false`) to
turn all three off.

## Approvals

When the network policy says `ask`, an approval modal appears: allow **once /
session / project / global**, or deny. It is laid out as labelled rows so the
decision is made from context rather than from a bare `host:port`:

```text
╭ Network request ───────────────────────────────────────────────╮
│ destination       crates.io:443                                │
│ protocol          TLS                                          │
│ address           13.226.34.10 — external                      │
│ requested by      cargo test --workspace --all-targets         │
│ why you're asked  no rule matches; default for external is ask │
│                                                                │
│ 3 endpoints are already saved for this project                 │
│                                                                │
│ o  once — just this request                                    │
│ s  session — every request here until this session ends        │
│ p  project — always allow here (saved for this repo)           │
│ g  global — always allow everywhere                            │
│ d  deny                                                        │
╰ press a key · Esc = deny ──────────────────────────────────────╯
```

Two things worth knowing about that layout. The **address class** is shown
because a hostname can hide it — a public name resolving to a private address is
what a DNS rebind looks like from here. And values are **truncated, never
wrapped**: a long command line must not be able to push the scope legend off the
bottom and leave you a prompt whose options you cannot read.

Everything in the modal is **display only**, and gathered *after* the verdict was
computed, so none of it can influence the decision. It exists because a prompt
with no context gets rubber-stamped, and a reflexively approved `ask` policy is an
`allow` policy that merely takes longer.

Credential prompts (mounting a credential file, injecting a credential env var)
use the same modal, titled `Credential access`.

Project/global choices persist host-side and merge into the policy on the next
run. See [Network egress](../security/network.md).

## Questions with options

The agent can ask you a multiple-choice question (the `ask_user` tool with
options): you get a selectable list and can still type a free-form answer.

## Copying text

Selected text is copied to the system clipboard via OSC 52, including through
`tmux`/`screen` (the escape sequence is wrapped in the right passthrough). Use
your terminal's selection (Shift often bypasses mouse capture for native
selection).

Mouse tracking is deliberately limited to button-held motion (`?1002`) rather
than every pointer movement (`?1003`): drag-selection needs the former, and the
latter turns an idle mouse into a steady stream of input the UI has no use for.

## If the UI stops responding

The TUI logs to `$TMPDIR/cowboy-<pid>.log` (stderr is redirected there so it
can't scribble over the screen). If input ever seems dead while the display keeps
updating, look for a line like:

```text
[input] 2106 byte(s) queued but unreported by crossterm; dropped 2106
```

That is the loop recovering from terminal input it was never told about — a burst
larger than crossterm's 1 KiB read buffer can leave bytes in the kernel queue
that its edge-triggered readiness never reports again. The status line says
`input recovered` when it happens. `Ctrl-L` forces a full repaint if a frame is
left with stale cells.

## Watching a ranch

`cowboy ranch watch <id>` opens a live dashboard for a Ranch Plan — a workstream
table, advance log, and keys to advance/refresh. See
[The dashboard](../ranch/dashboard.md).
