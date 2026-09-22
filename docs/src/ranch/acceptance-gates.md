# Acceptance gates

Every finished workstream pauses for explicit human sign-off. Session completion
alone never marks the workstream complete or unblocks its dependents.

## When the gate holds

When a workstream's session completes (or becomes stale), reconciliation moves it
to `WaitingForUser` and attempts to promote its selected artifacts and handoff for
review. The ranch also becomes `WaitingForUser` when nothing else is running.
Dependents remain blocked whether or not the workstream declares `acceptance` or
`expected_artifacts`.

Those fields guide the worker and reviewer; Cowboy does not currently machine-check
that named expected artifacts were published. A failed review promotion is reported
and leaves the workstream waiting without unblocking anything.

## Signing off

```sh
cowboy ranch accept billing schema     # publish, mark complete, unblock deps
```

`accept` is the acceptance-gate sign-off; `complete` is the same operation under a
more general name (use either). Both publish first: Cowboy stages the complete set
of selected artifacts plus the optional handoff, flushes it, then atomically installs
that directory snapshot. Publication is all-or-nothing at the directory boundary, so
no partial snapshot becomes visible; a failure before installation preserves the
previous snapshot. Any publication or final durability error leaves the workstream
not complete and its dependents blocked, even if the atomic switch already became
visible and must be retried. Only after promotion succeeds does Cowboy persist
`Complete`, recompute readiness, and unblock dependents. A failure to save that Ranch
progress likewise leaves dependents blocked on disk.

After a successful sign-off, run `cowboy ranch start billing` (or rely on the
[coordinator](coordination.md)) to launch the newly-ready workstreams.

## Why it composes with auto-advance

Auto-advance + acceptance gating mean the coordinator starts ready work on its own,
but **stops for you after every workstream** before dependent work begins. You stay
in control of what "done" means.
