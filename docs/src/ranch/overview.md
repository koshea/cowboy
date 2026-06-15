# Ranch Plans — overview

A **Ranch Plan** turns one large task into multiple coordinated, dependency-aware
**workstreams**. Each workstream is a normal Cowboy session running in its own git
worktree/branch, so it inherits the full security boundary and the one-writable-
session-per-worktree lease. A host-side **coordinator** tracks dependencies,
promotes outputs, advances the plan, and pauses for your sign-off where it matters.

> Coordination is **artifact-driven, not chat-driven**: a workstream publishes
> artifacts and a handoff; downstream workstreams consume them. There is no
> free-form agent↔agent chat.

## The model

- A **ranch** has a goal, a status, and a list of workstreams. Its plan lives at
  `.cowboy/ranches/<id>/ranch.yaml` — the **committed source of truth**.
- A **workstream** has an id, goal, `depends_on`, optional `expected_artifacts`,
  and optional `acceptance` criteria. Its status tracks the dependency graph
  (Planned → Blocked → Ready → Running → Complete, with WaitingForUser/Failed/…).
- **Readiness** is computed from the graph: a workstream is `Ready` when all its
  dependencies are done.

## Invariants

- **The agent never edits `ranch.yaml`.** Scope changes go through a
  [proposal you approve](scope-proposals.md).
- **Artifacts are promoted, not shared ad hoc.** A finished workstream's published
  artifacts (+ handoff) are copied into the committed ranch store at
  `.cowboy/ranches/<id>/artifacts/<workstream>/`, and injected into the prompts of
  dependents.
- **Acceptance gates pause for humans.** A workstream that declares acceptance
  criteria (or didn't publish its expected artifacts) waits for your
  [sign-off](acceptance-gates.md) instead of auto-completing.

## What lives where

```
.cowboy/ranches/<id>/
  ranch.yaml                    # COMMITTED — the plan (source of truth)
  artifacts/<workstream>/...    # COMMITTED — promoted outputs
  proposals/<pid>.yaml          # COMMITTED — scope-change proposals (audit trail)
```

Continue with [Creating & running a ranch](running.md).
