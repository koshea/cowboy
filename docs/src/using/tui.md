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
  subagents, **Alt-w** watches one, **Alt-s** stops them, **Alt-d** detaches.
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

## Approvals

When the network policy says `ask`, an approval modal appears: allow **once /
session / project / global**, or deny. It names the command that wants the
destination, not just the destination. Project/global choices persist host-side and
merge into the policy on the next run. See [Network egress](../security/network.md).

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
