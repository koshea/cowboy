# Acceptance gates

A finished workstream does **not** always auto-complete. It is held for human
sign-off when it can't be verified automatically — so the dependency graph pauses
exactly where your judgment matters.

## When the gate holds

When a workstream's session completes, the reconcile step checks:

- **Acceptance criteria** — if the workstream declared any `acceptance:` items
  (which are human-readable and can't be auto-verified), it is gated.
- **Expected artifacts** — if it declared `expected_artifacts` but didn't actually
  publish all of them, it is gated.

A workstream with **neither** auto-completes as before.

A gated workstream goes to status `WaitingForUser` (and the ranch to
`WaitingForUser`). Its artifacts are **still promoted** so you can review them, but
it does **not** unblock its dependents until you sign off.

## Signing off

```sh
cowboy ranch accept billing schema     # verify, mark complete, promote, unblock deps
```

`accept` is the acceptance-gate sign-off; `complete` is the same operation under a
more general name (use either). After it, run `cowboy ranch start billing` (or rely
on the [coordinator](coordination.md)) to launch the newly-unblocked workstreams.

## Why it composes with auto-advance

Auto-advance + acceptance gating together mean the coordinator runs the plan
hands-off through the parts that are safe, and **stops for you** at every point
that declares acceptance criteria. You stay in control of what "done" means.
