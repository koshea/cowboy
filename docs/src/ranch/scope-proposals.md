# Scope-change proposals

The ranch plan (`ranch.yaml`) is the committed source of truth and is **never**
edited by a worker agent — nor autonomously by the coordinator. When the plan
looks wrong, the change is filed as a **proposal** that you review and approve or
reject. Only on approval does the plan change.

## Filing a proposal

You can file one yourself:

```sh
cowboy ranch propose billing --summary "need a caching layer" \
  --rationale "the API is too slow without it" \
  --add-workstream cache --title Cache --goal "add caching" --depends-on api

cowboy ranch propose billing --summary "drop the legacy import" --remove-workstream legacy
cowboy ranch propose billing --summary "consider rate limiting" --note
```

And a workstream agent that discovers a problem files one through the
`propose_scope_change` tool instead of diverging from the plan — its coordination
rules tell it to. Agent-filed proposals land in the same store, marked with the
workstream they came from.

## Reviewing & deciding

```sh
cowboy ranch proposals billing          # pending proposals
cowboy ranch proposals billing --all    # include decided ones
cowboy ranch approve billing p0001      # apply the change to the plan
cowboy ranch reject  billing p0002 --reason "out of scope for now"
```

Proposals are stored at `.cowboy/ranches/<id>/proposals/<pid>.yaml` (committed —
an audit trail of how the plan evolved).

## Safe application

`approve` applies the change and guards against unsafe edits:

- **add_workstream** — refuses a duplicate id; the new workstream starts
  `Planned` and readiness is recomputed.
- **remove_workstream** — refuses to remove a workstream that has started or is
  done, or one that another workstream still depends on.
- **note** — recorded for the record; no automatic edit.
