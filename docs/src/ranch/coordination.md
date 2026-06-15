# Auto-advancing coordinator

By default (`auto_advance: true`), the daemon advances a ranch on its own — you
don't have to re-run `cowboy ranch start` as workstreams finish.

## How it works

When a ranch workstream's session reaches a terminal state (completed, failed, or
a crashed/stale exit), the daemon's coordinator:

1. resolves the ranch and the worktree's **main repo** (via `git-common-dir`);
2. checks the ranch's `auto_advance` flag (skips if you turned it off);
3. spawns `cowboy ranch start <id>` against the main repo.

That reuses the exact, tested advance path out-of-band, so it never blocks the
daemon. The result: a workstream completes → its outputs are promoted → its
dependents unblock → they launch — automatically.

## Bursts and races

A per-ranch **in-flight guard** coalesces bursts: if several workstreams finish
close together, only one advance runs at a time. A **dirty flag** re-runs the
advance once if another workstream finished while it was running, so no completion
is missed.

## Turning it off

Set `auto_advance: false` in `ranch.yaml` to drive the plan manually with `cowboy
ranch start` (or the [dashboard](dashboard.md)'s `s` key). This is useful when you
want to inspect each step before the next workstream launches.

Note that **acceptance gates still pause** regardless of `auto_advance`: the
coordinator advances through the safe parts and stops for your
[sign-off](acceptance-gates.md) where a workstream declares acceptance criteria.
