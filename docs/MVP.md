# cowboy — MVP overview

`cowboy` is a local coding agent that runs the AI inside a Docker container
while the **host** enforces security at the container and network layer. The
agent is never part of the security boundary.

> The agent can run wild because the runtime owns the corral.

## Commands

```
cowboy                     # interactive session (ratatui TUI on a terminal)
cowboy "fix the tests"     # one-shot task
cowboy init [--git]        # write .cowboy/{security,agent,models}.yaml
cowboy doctor              # check Docker, Linux, nft, model config, Compose
cowboy run <cmd>           # run a command in the agent container
cowboy shell               # interactive shell in the agent container
cowboy patch show|save|apply|check|revert
cowboy proc list|start|stop|restart|logs <name>
cowboy logs                # list past sessions
cowboy replay <id>         # replay a past session
```

## How a session works

1. `cowboy` loads host-owned `security.yaml` (never mounted into the container).
2. It builds/starts the agent container with the project mounted at `/workspace`
   and the host-owned config **masked**.
3. With isolation enabled (default), it brings up the sole-egress network gateway
   and forces the agent's only route out through it (see [NETWORK.md](NETWORK.md)).
4. The agent loop calls an OpenAI-compatible model with a minimal tool surface:
   `shell`, `final`, `ask_user`. All cowboy capabilities (`patch`, `proc`) are
   CLIs the agent calls *through* `shell`.
5. Shell commands run in the container; output is streamed to the UI and fed
   back to the model. The session is logged under `.cowboy/sessions/<id>/`.

## Status / known MVP limitations

- Linux-only; macOS/Windows are out of scope for the MVP.
- The default agent image (`cowboy/agent:local`) is built locally on first run.
- The TUI network-approval modal renders, but the host-side control-socket
  server that feeds live "ask" prompts is a follow-up; until then unknown egress
  fails closed (denied) — the boundary is intact.
- Token-aware context compaction (tiktoken) is a follow-up; output is truncated
  by byte budget.
