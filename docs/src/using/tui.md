# The TUI

On a terminal, `cowboy` runs a ratatui-based terminal UI that streams the agent's
work turn by turn.

## Layout & interaction

- A scrollable transcript pane shows the conversation, tool calls, and streamed
  command output; the input box at the bottom grows up to 5 lines as you type.
- **Enter** sends; multi-line input is supported (paste arrives as one chunk).
- **Ctrl-C** opens the interrupt menu: `k` cancels the current turn (you keep
  going), `e` ends the session.
- **Scrolling** follows the tail by exact wrapped-line counts, so the latest
  output is never cut off under the input box.

## Approvals

When the network policy says `ask`, an approval modal appears: allow **once /
session / project / global**, or deny. Project/global choices persist to
`.cowboy/approvals.json` and merge into the policy on the next run. See
[Network gateway](../security/network.md).

## Questions with options

The agent can ask you a multiple-choice question (the `ask_user` tool with
options): you get a selectable list and can still type a free-form answer.

## Copying text

Selected text is copied to the system clipboard via OSC 52, including through
`tmux`/`screen` (the escape sequence is wrapped in the right passthrough). Use
your terminal's selection (Shift often bypasses mouse capture for native
selection).

## Watching a ranch

`cowboy ranch watch <id>` opens a live dashboard for a Ranch Plan — a workstream
table, advance log, and keys to advance/refresh. See
[The dashboard](../ranch/dashboard.md).
