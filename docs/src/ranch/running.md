# Creating & running a ranch

## Create

```sh
cowboy ranch create "Billing system" --goal "Add billing end-to-end"
```

This writes a skeleton `.cowboy/ranches/<id>/ranch.yaml` with commented examples.
Edit it to define workstreams: each gets an `id`, `title`, `goal`, `depends_on`,
and optionally `expected_artifacts` and `acceptance` criteria.

```yaml
version: 1
id: billing
title: Billing system
goal: Add billing end-to-end
status: planning
auto_advance: true            # daemon launches ready workstreams as deps finish
workstreams:
  - id: schema
    title: Add billing schema
    goal: Add tables + migrations for billing.
    depends_on: []
    expected_artifacts: [schema-contract.md]
    acceptance:
      - migrations apply cleanly
  - id: api
    title: Implement billing API
    depends_on: [schema]
    expected_artifacts: [api-contract.md]
```

## Inspect

```sh
cowboy ranch status            # list all ranches
cowboy ranch status billing    # one ranch: workstream table + what's ready
```

## Start

```sh
cowboy ranch start billing
```

`start` is **idempotent and re-entrant**. Each run:

1. reconciles already-started workstreams from their live session status;
2. promotes the outputs of any that just finished;
3. launches every newly-**ready** workstream in its own `cowboy/<ranch>-<ws>`
   worktree/branch, tagging the session with the ranch + workstream;
4. saves the updated plan.

Run it again as workstreams complete to advance the graph — or let the
[coordinator](coordination.md) do it for you (the default).

## Attach & finish

```sh
cowboy ranch attach billing schema     # attach the TUI to a workstream's session
cowboy ranch complete billing schema   # manually mark a workstream done + promote + unblock
```

Each workstream worker is **one-shot**: it runs its seeded task and then ends, so
its session goes `Completed` and the plan can advance. The worker's task prompt
includes the workstream goal, its dependencies' promoted artifacts (inline),
expected artifacts, acceptance criteria, and the coordination rules.
