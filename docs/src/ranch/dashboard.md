# The dashboard

```sh
cowboy ranch watch billing
```

`ranch watch` opens a live TUI dashboard for a ranch.

## What it shows

- **Header** — the ranch id/title, overall status (color-coded), and goal.
- **Workstream table** — each workstream's id, status (colored by state), its
  session id with the *live* session status, and its dependencies.
- **Advance log** — the output of the last advance (collapses when empty).
- **Footer** — the key hints.

The view auto-refreshes on a 1-second poll, reflecting the dependency graph as a
non-saving snapshot (it reconciles in memory for display).

## Keys

| Key | Action |
|-----|--------|
| `s` | Advance the plan now (reconcile finished workstreams + launch any newly ready) |
| `r` | Refresh the snapshot |
| `q` / `Esc` | Quit |

Advancing from the dashboard runs the same logic as `cowboy ranch start`, but its
output is rendered into the log pane rather than printed, so it never disturbs the
raw-mode terminal.

For sign-off on a gated workstream, drop back to the CLI:
`cowboy ranch accept <id> <workstream>` (see [Acceptance gates](acceptance-gates.md)).
